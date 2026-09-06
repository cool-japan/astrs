//! Parser tests, written against the public surface so they describe
//! behaviour rather than internal structure.
//!
//! The compatibility suites in `tests/` prove this parser agrees with
//! `serde_yaml`; these prove the things that agreement cannot cover — where
//! an error points, what a limit does, and the handful of constructs where
//! this crate deliberately does something better.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::error::ErrorKind;
use crate::limits::Limits;
use crate::mapping::Mapping;
use crate::value::{Tag, Value};

use super::parse_documents;

fn parse(input: &str) -> Value {
    let mut documents = parse_documents(input, Limits::default())
        .unwrap_or_else(|error| panic!("{input:?}: {error}"));
    assert_eq!(documents.len(), 1, "{input:?} produced {documents:?}");
    documents.remove(0)
}

fn parse_error(input: &str) -> crate::error::Error {
    parse_documents(input, Limits::default()).expect_err(&format!("{input:?} should not parse"))
}

fn map(pairs: &[(&str, Value)]) -> Value {
    let mut mapping = Mapping::new();
    for (key, value) in pairs {
        mapping.insert(Value::from(*key), value.clone());
    }
    Value::Mapping(mapping)
}

fn seq(items: &[Value]) -> Value {
    Value::Sequence(items.to_vec())
}

// ------------------------------------------------------------- structure

#[test]
fn a_manifest_shaped_document_parses_into_the_expected_tree() {
    let value = parse(concat!(
        "name: perception\n",
        "nodes:\n",
        "  - id: camera\n",
        "    path: ./camera\n",
        "    outputs: [frames]\n",
        "    env: { CAMERA_INDEX: 0 }\n",
    ));
    assert_eq!(
        value.get("name").and_then(Value::as_str),
        Some("perception")
    );
    let nodes = value
        .get("nodes")
        .and_then(Value::as_sequence)
        .expect("nodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].get("id").and_then(Value::as_str), Some("camera"));
    assert_eq!(
        nodes[0].get("outputs"),
        Some(&seq(&[Value::from("frames")]))
    );
    assert_eq!(
        nodes[0].get("env").and_then(|env| env.get("CAMERA_INDEX")),
        Some(&Value::from(0u64))
    );
}

#[test]
fn a_sequence_may_share_its_keys_indentation_but_not_its_dashs() {
    // A block sequence under a key may sit at the key's own column ...
    assert_eq!(
        parse("a:\n- 1\n- 2\n"),
        map(&[("a", seq(&[Value::from(1u64), Value::from(2u64)]))])
    );
    // ... or be indented, which means the same thing.
    assert_eq!(
        parse("a:\n  - 1\n  - 2\n"),
        map(&[("a", seq(&[Value::from(1u64), Value::from(2u64)]))])
    );
    // But an empty entry followed by a sibling `-` is two entries, not a
    // nested sequence.
    assert_eq!(parse("- \n- b\n"), seq(&[Value::Null, Value::from("b")]));
    assert_eq!(parse("-\n- b\n"), seq(&[Value::Null, Value::from("b")]));
}

#[test]
fn compact_entries_start_a_collection_on_the_markers_own_line() {
    assert_eq!(
        parse("- id: camera\n  path: ./x\n"),
        seq(&[map(&[
            ("id", Value::from("camera")),
            ("path", Value::from("./x")),
        ])])
    );
    assert_eq!(
        parse("- - 1\n  - 2\n"),
        seq(&[seq(&[Value::from(1u64), Value::from(2u64)])])
    );
    assert_eq!(
        parse("? - 1\n  - 2\n: v\n"),
        Value::Mapping(
            [(
                seq(&[Value::from(1u64), Value::from(2u64)]),
                Value::from("v")
            )]
            .into_iter()
            .collect()
        )
    );
}

#[test]
fn a_plain_scalar_folds_its_continuation_lines() {
    assert_eq!(
        parse("a: one\n  two\n  three\n"),
        map(&[("a", Value::from("one two three"))])
    );
    assert_eq!(
        parse("a: one\n\n  two\n"),
        map(&[("a", Value::from("one\ntwo"))])
    );
    // The fold stops at the next sibling key.
    assert_eq!(
        parse("a: one\n  two\nb: 1\n"),
        map(&[("a", Value::from("one two")), ("b", Value::from(1u64))])
    );
}

#[test]
fn quotes_and_brackets_are_only_indicators_where_a_node_starts() {
    assert_eq!(parse("a\"b: 1\n"), map(&[("a\"b", Value::from(1u64))]));
    assert_eq!(parse("a[b: 1\n"), map(&[("a[b", Value::from(1u64))]));
    assert_eq!(parse("a'b: 1\n"), map(&[("a'b", Value::from(1u64))]));
    assert_eq!(
        parse("http://x: 1\n"),
        map(&[("http://x", Value::from(1u64))])
    );
    // At the start they are, though.
    assert_eq!(parse("\"a b\": 1\n"), map(&[("a b", Value::from(1u64))]));
}

#[test]
fn flow_collections_may_span_lines_and_carry_comments() {
    assert_eq!(
        parse("a: [\n  1, # one\n  2,\n]\n"),
        map(&[("a", seq(&[Value::from(1u64), Value::from(2u64)]))])
    );
    assert_eq!(
        parse("a: {\n  b: 1,\n}\n"),
        map(&[("a", map(&[("b", Value::from(1u64))]))])
    );
}

#[test]
fn documents_are_separated_by_markers() {
    let documents = parse_documents("a: 1\n---\nb: 2\n...\n", Limits::default()).expect("stream");
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0], map(&[("a", Value::from(1u64))]));
    assert_eq!(documents[1], map(&[("b", Value::from(2u64))]));

    // Anchors do not leak across a document boundary.
    let error = parse_error("a: &x 1\n---\nb: *x\n");
    assert!(matches!(error.kind(), ErrorKind::UnknownAnchor { .. }));
}

#[test]
fn directives_are_read_and_tag_handles_resolve_through_them() {
    assert_eq!(
        parse("%YAML 1.2\n---\na: 1\n"),
        map(&[("a", Value::from(1u64))])
    );
    // `%TAG` maps a handle onto a prefix; a core-schema URI reached that way
    // types the scalar exactly as `!!str` would.
    assert_eq!(
        parse("%TAG !y! tag:yaml.org,2002:\n---\n!y!str 1\n"),
        Value::from("1")
    );
    let unknown = parse_error("!e!foo bar\n");
    assert!(matches!(unknown.kind(), ErrorKind::UnknownTagHandle { .. }));
    let bad_version = parse_error("%YAML 2.0\n---\na: 1\n");
    assert!(matches!(
        bad_version.kind(),
        ErrorKind::InvalidDirective { .. }
    ));
    // An unknown directive is reserved, and must be ignored rather than
    // rejected.
    assert_eq!(
        parse("%FUTURE whatever\n---\na: 1\n"),
        map(&[("a", Value::from(1u64))])
    );
}

// ---------------------------------------------------------------- scalars

#[test]
fn block_scalars_honour_every_indicator() {
    assert_eq!(
        parse("a: |\n  x\n  y\n"),
        map(&[("a", Value::from("x\ny\n"))])
    );
    assert_eq!(parse("a: |-\n  x\n"), map(&[("a", Value::from("x"))]));
    assert_eq!(parse("a: |+\n  x\n\n"), map(&[("a", Value::from("x\n\n"))]));
    assert_eq!(
        parse("a: >\n  x\n  y\n"),
        map(&[("a", Value::from("x y\n"))])
    );
    assert_eq!(
        parse("a: >\n  x\n   more\n  y\n"),
        map(&[("a", Value::from("x\n more\ny\n"))])
    );
    assert_eq!(parse("a: |2\n   x\n"), map(&[("a", Value::from(" x\n"))]));
    assert_eq!(parse("a: |\n"), map(&[("a", Value::from(""))]));
    // A comment is allowed on the header line.
    assert_eq!(
        parse("a: | # why\n  x\n"),
        map(&[("a", Value::from("x\n"))])
    );
}

#[test]
fn a_block_scalar_ends_at_the_first_less_indented_line() {
    assert_eq!(
        parse("a: |\n  x\nb: 1\n"),
        map(&[("a", Value::from("x\n")), ("b", Value::from(1u64))])
    );
}

#[test]
fn double_quoted_escapes_cover_the_whole_table() {
    let value = parse(concat!(
        r#"a: "\0\a\b\t\n\v\f\r\e\ \"\/\\\N\_\L\P""#,
        "\n",
        r#"b: "\x41\u00e9\U0001F600""#,
        "\n",
        "c: \"one\\\n   two\"\n",
    ));
    assert_eq!(
        value.get("a").and_then(Value::as_str),
        Some("\0\u{7}\u{8}\t\n\u{b}\u{c}\r\u{1b} \"/\\\u{85}\u{a0}\u{2028}\u{2029}")
    );
    assert_eq!(value.get("b").and_then(Value::as_str), Some("Aé😀"));
    assert_eq!(value.get("c").and_then(Value::as_str), Some("onetwo"));
}

#[test]
fn a_bad_escape_points_at_the_backslash() {
    let error = parse_error("a: \"x\\q\"\n");
    assert!(matches!(
        error.kind(),
        ErrorKind::UnknownEscape { found: 'q' }
    ));
    assert_eq!((error.line(), error.column()), (Some(1), Some(6)));

    let truncated = parse_error("a: \"\\u12\"\n");
    assert!(matches!(
        truncated.kind(),
        ErrorKind::InvalidEscapeValue { .. }
    ));

    let surrogate = parse_error("a: \"\\uD800\"\n");
    assert!(matches!(
        surrogate.kind(),
        ErrorKind::InvalidEscapeValue { .. }
    ));
}

// -------------------------------------------------------- anchors & tags

#[test]
fn an_alias_copies_the_anchored_subtree() {
    let value = parse("base: &b {x: 1}\ncopy: *b\n");
    assert_eq!(value.get("copy"), value.get("base"));

    // A later anchor of the same name wins for later aliases.
    let value = parse("a: &x 1\nb: &x 2\nc: *x\n");
    assert_eq!(value.get("c"), Some(&Value::from(2u64)));
}

#[test]
fn an_unknown_alias_points_at_the_star() {
    let error = parse_error("a: 1\nb: *missing\n");
    assert!(matches!(error.kind(), ErrorKind::UnknownAnchor { .. }));
    assert_eq!((error.line(), error.column()), (Some(2), Some(4)));
}

#[test]
fn a_local_tag_survives_and_a_core_tag_types_the_scalar() {
    let value = parse("a: !Circle 3\n");
    let tagged = value.get("a").expect("a");
    assert_eq!(tagged.tag().map(Tag::as_str), Some("Circle"));
    assert_eq!(tagged.untagged(), &Value::from(3u64));

    assert_eq!(parse("a: !!str 1\n"), map(&[("a", Value::from("1"))]));
    assert_eq!(parse("a: !!int \"7\"\n"), map(&[("a", Value::from(7u64))]));
    assert_eq!(parse("a: !!float 2\n"), map(&[("a", Value::from(2.0))]));
    assert_eq!(parse("a: !!str\n"), map(&[("a", Value::from(""))]));

    let mismatch = parse_error("a: !!bool yes\n");
    assert!(matches!(mismatch.kind(), ErrorKind::TagMismatch { .. }));
}

#[test]
fn a_tag_on_one_line_types_the_block_below_it() {
    let value = parse("a: !Wrapper\n  b: 1\n");
    let tagged = value.get("a").expect("a");
    assert_eq!(tagged.tag().map(Tag::as_str), Some("Wrapper"));
    assert_eq!(tagged.untagged().get("b"), Some(&Value::from(1u64)));

    // Both tags apply when the inner node names one of its own.
    let value = parse("a: !Outer\n  !Inner 1\n");
    let outer = value.get("a").expect("a");
    assert_eq!(outer.tag().map(Tag::as_str), Some("Outer"));
    assert_eq!(
        outer.untagged(),
        &Value::from(1u64),
        "the inner tag's payload survives"
    );
}

#[test]
fn a_malformed_tag_is_reported_rather_than_guessed() {
    assert!(matches!(
        parse_error("a: !<unterminated\n").kind(),
        ErrorKind::InvalidTag { .. }
    ));
    assert!(matches!(
        parse_error("a: !<>x\n").kind(),
        ErrorKind::InvalidTag { .. }
    ));
    assert!(matches!(
        parse_error("a: !! x\n").kind(),
        ErrorKind::InvalidTag { .. }
    ));
}

// ----------------------------------------------------------------- errors

#[test]
fn a_duplicate_key_points_at_the_repeat() {
    let error = parse_error("a: 1\nb: 2\na: 3\n");
    let ErrorKind::DuplicateKey { key } = error.kind() else {
        panic!("{error}");
    };
    assert_eq!(key, "a");
    assert_eq!((error.line(), error.column()), (Some(3), Some(1)));

    // Flow mappings are checked too, and a non-string key renders readably.
    let flow = parse_error("{1: a, 1: b}\n");
    assert!(matches!(flow.kind(), ErrorKind::DuplicateKey { .. }));
}

#[test]
fn a_tab_used_for_indentation_points_at_the_tab() {
    let error = parse_error("nodes:\n\t- id: camera\n");
    assert!(matches!(error.kind(), ErrorKind::TabInIndentation));
    assert_eq!((error.line(), error.column()), (Some(2), Some(1)));
    // A tab *inside* a scalar is content, not indentation.
    assert_eq!(parse("a: b\tc\n"), map(&[("a", Value::from("b\tc"))]));
}

#[test]
fn a_stray_mapping_value_is_reported_where_the_colon_is() {
    let error = parse_error("a: b: c\n");
    assert!(matches!(error.kind(), ErrorKind::MappingValueNotAllowed));
    assert_eq!((error.line(), error.column()), (Some(1), Some(5)));
}

#[test]
fn an_unclosed_flow_collection_points_at_its_opening_bracket() {
    let error = parse_error("nodes: [a, b\n");
    assert!(matches!(
        error.kind(),
        ErrorKind::UnclosedFlow { expected: ']' }
    ));
    assert_eq!((error.line(), error.column()), (Some(1), Some(8)));

    let brace = parse_error("nodes: {a: 1\n");
    assert!(matches!(
        brace.kind(),
        ErrorKind::UnclosedFlow { expected: '}' }
    ));
}

#[test]
fn an_unterminated_quote_points_at_the_end_of_the_input() {
    let error = parse_error("a: \"open\n");
    assert!(matches!(
        error.kind(),
        ErrorKind::UnexpectedEndOfInput {
            context: "a double-quoted scalar"
        }
    ));
    let single = parse_error("a: 'open\n");
    assert!(matches!(
        single.kind(),
        ErrorKind::UnexpectedEndOfInput {
            context: "a single-quoted scalar"
        }
    ));
}

#[test]
fn misaligned_block_entries_are_reported_with_both_columns() {
    let error = parse_error("a: 1\n  b: 2\n");
    // The continuation folds into `a`'s scalar and then trips over the colon.
    assert!(matches!(error.kind(), ErrorKind::MappingValueNotAllowed));

    let error = parse_error("a:\n  b: 1\n c: 2\n");
    let ErrorKind::InvalidIndentation { expected, found } = error.kind() else {
        panic!("{error}");
    };
    assert_eq!((*expected, *found), (0, 1));
}

#[test]
fn reserved_indicators_cannot_start_a_node() {
    for input in ["@invalid\n", "`invalid\n", "a: @x\n", "a: `x\n"] {
        assert!(
            matches!(
                parse_error(input).kind(),
                ErrorKind::UnexpectedCharacter { .. }
            ),
            "{input:?}"
        );
    }
}

#[test]
fn a_block_sequence_may_not_start_on_a_keys_own_line() {
    let error = parse_error("a: - 1\n");
    assert!(matches!(
        error.kind(),
        ErrorKind::UnexpectedCharacter { found: '-', .. }
    ));
}

#[test]
fn leftover_content_after_a_document_is_reported() {
    let error = parse_error("- a\nb: 1\n");
    assert!(matches!(
        error.kind(),
        ErrorKind::UnexpectedCharacter {
            context: "the end of a document",
            ..
        }
    ));
}

// ----------------------------------------------------------------- limits

#[test]
fn the_depth_limit_refuses_before_it_recurses() {
    let flow = format!("{}{}", "[".repeat(129), "]".repeat(129));
    let error = parse_documents(&flow, Limits::default()).expect_err("too deep");
    assert!(error.is_limit());
    assert!(matches!(
        error.kind(),
        ErrorKind::DepthLimitExceeded { limit: 128 }
    ));

    // Exactly at the limit is fine.
    let at_limit = format!("{}{}", "[".repeat(128), "]".repeat(128));
    assert!(parse_documents(&at_limit, Limits::default()).is_ok());

    // Block collections are counted the same way.
    let block: String = (0..129)
        .map(|i| format!("{}a:\n", "  ".repeat(i)))
        .collect();
    assert!(
        parse_documents(&block, Limits::default())
            .expect_err("too deep")
            .is_limit()
    );
}

#[test]
fn the_alias_budget_stops_a_billion_laughs_bomb() {
    let bomb = "a: &a [x,x,x,x,x,x,x,x,x]\n\
                b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\n\
                c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\n\
                d: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n\
                e: &e [*d,*d,*d,*d,*d,*d,*d,*d,*d]\n\
                f: &f [*e,*e,*e,*e,*e,*e,*e,*e,*e]\n\
                g: &g [*f,*f,*f,*f,*f,*f,*f,*f,*f]\n\
                h: &h [*g,*g,*g,*g,*g,*g,*g,*g,*g]\n\
                i: &i [*h,*h,*h,*h,*h,*h,*h,*h,*h]\n\
                boom: [*i,*i,*i,*i,*i,*i,*i,*i,*i]\n";
    let error = parse_documents(bomb, Limits::default()).expect_err("refused");
    assert!(error.is_limit());
    assert!(matches!(
        error.kind(),
        ErrorKind::AliasBudgetExhausted { limit: 1_000_000 }
    ));

    // The budget is charged per expansion, not per anchor: a document that
    // aliases the same small node many times is fine.
    let benign = format!(
        "a: &a [1, 2, 3]\nuses: [{}]\n",
        (0..500).map(|_| "*a").collect::<Vec<_>>().join(", ")
    );
    assert!(parse_documents(&benign, Limits::default()).is_ok());
    assert!(
        parse_documents(&benign, Limits::default().with_max_alias_nodes(100))
            .expect_err("budget")
            .is_limit()
    );
}

#[test]
fn the_input_size_limit_is_checked_before_anything_is_scanned() {
    let error =
        parse_documents("a: 1\n", Limits::default().with_max_input_bytes(2)).expect_err("too big");
    assert!(error.is_limit());
    let ErrorKind::InputTooLarge { limit, found } = error.kind() else {
        panic!("{error}");
    };
    assert_eq!((*limit, *found), (2, 5));
    assert_eq!(error.span(), None);
}

#[test]
fn normalization_handles_windows_line_endings_and_a_byte_order_mark() {
    assert_eq!(
        parse("\u{feff}a: 1\r\nb: 2\r\n"),
        map(&[("a", Value::from(1u64)), ("b", Value::from(2u64))])
    );
    // Positions are still counted against the normalized text, which has the
    // same line numbering.
    let error = parse_error("a: 1\r\n\tb: 2\r\n");
    assert_eq!(error.line(), Some(2));
}
