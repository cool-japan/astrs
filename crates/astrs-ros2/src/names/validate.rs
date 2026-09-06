//! The ROS 2 naming rules, as predicates over a `&str`.
//!
//! Every rule here comes from the same three places `rmw`'s
//! `rmw_validate_*_name` functions implement, and each is checked
//! separately so a rejection can say *which* one broke and *where* — a
//! message like "`/my node` is not a valid topic name: the character ' ' at
//! offset 3 is not one of the …" is worth several times a bare `false`.
//!
//! # The rules
//!
//! | Rule | Node name | Namespace | Topic / service |
//! |---|---|---|---|
//! | may be empty | no | no | no |
//! | may contain `/` | no | yes | yes |
//! | must be absolute | — | yes | only once expanded |
//! | may contain `~` | no | no | yes, at the start only |
//! | may contain `{sub}` | no | no | yes |
//! | token may start with a digit | no | no | no |
//! | may end with `/` | — | no (except the root `/`) | no |
//! | may contain `//` | — | no | no |
//!
//! # Hidden names
//!
//! A token beginning with `_` marks a name as *hidden*: `ros2 topic list`
//! omits it unless asked, and `/_ros2cli_12345` is the canonical example.
//! [`is_hidden`] answers that question without rejecting anything —
//! hiddenness is a display property, never a validity one.

use crate::error::NameFault;

/// The longest fully-qualified ROS 2 name this crate accepts.
///
/// `rmw` caps a *DDS* topic name at 256 octets, and the longest mangled form
/// is `rq/` + name + `Request` — ten octets of overhead. Rejecting the ROS
/// name at 246 rather than letting the mangled one overflow is what turns a
/// truncated discovery announcement into an error at the call that named it.
pub const MAX_NAME_LEN: usize = 246;

/// The longest node name this crate accepts.
pub const MAX_NODE_NAME_LEN: usize = 128;

/// The root namespace.
pub const ROOT_NAMESPACE: &str = "/";

/// True when `character` may appear in a ROS 2 name at all.
///
/// The union of every position's alphabet; position-specific rules —
/// `~` only at the start, `/` never in a node name — are checked separately.
#[must_use]
pub const fn is_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '/' | '~' | '{' | '}')
}

/// True when `character` may appear inside a single token.
#[must_use]
pub const fn is_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// Validate one slash-separated token: non-empty, alphanumerics and `_`,
/// never starting with a digit.
///
/// `offset` is the token's byte offset in the enclosing name, used only to
/// make the returned fault point at the right place.
///
/// # Errors
///
/// [`NameFault::EmptyToken`], [`NameFault::IllegalCharacter`] or
/// [`NameFault::TokenStartsWithDigit`].
pub fn validate_token(token: &str, offset: usize) -> Result<(), NameFault> {
    let mut characters = token.char_indices();
    let Some((_, first)) = characters.next() else {
        return Err(NameFault::EmptyToken { offset });
    };
    if first.is_ascii_digit() {
        return Err(NameFault::TokenStartsWithDigit { offset });
    }
    if !is_token_character(first) {
        return Err(NameFault::IllegalCharacter {
            character: first,
            offset,
        });
    }
    for (index, character) in characters {
        if !is_token_character(character) {
            return Err(NameFault::IllegalCharacter {
                character,
                offset: offset.saturating_add(index),
            });
        }
    }
    Ok(())
}

/// Validate a node name: one token, no slashes, no substitutions.
///
/// # Errors
///
/// [`NameFault::Empty`], [`NameFault::UnexpectedSlash`], or whatever
/// [`validate_token`] reports.
pub fn validate_node_name(name: &str) -> Result<(), NameFault> {
    if name.is_empty() {
        return Err(NameFault::Empty);
    }
    if let Some(offset) = name.find('/') {
        return Err(NameFault::UnexpectedSlash { offset });
    }
    if let Some(offset) = name.find('~') {
        return Err(NameFault::MisplacedTilde { offset });
    }
    if let Some(offset) = name.find(['{', '}']) {
        return Err(NameFault::MalformedSubstitution { offset });
    }
    validate_token(name, 0)
}

/// Validate a node namespace: absolute, no trailing slash, valid tokens.
///
/// The root namespace `/` is the one name that is allowed to be a bare
/// slash; every other namespace is `/` followed by one or more tokens.
///
/// # Errors
///
/// [`NameFault::Empty`], [`NameFault::NotAbsolute`],
/// [`NameFault::MisplacedTilde`], or whatever [`validate_token`] reports.
pub fn validate_namespace(namespace: &str) -> Result<(), NameFault> {
    if namespace.is_empty() {
        return Err(NameFault::Empty);
    }
    if namespace == ROOT_NAMESPACE {
        return Ok(());
    }
    if !namespace.starts_with('/') {
        return Err(NameFault::NotAbsolute);
    }
    if let Some(offset) = namespace.find('~') {
        return Err(NameFault::MisplacedTilde { offset });
    }
    if let Some(offset) = namespace.find(['{', '}']) {
        return Err(NameFault::MalformedSubstitution { offset });
    }
    validate_token_path(namespace)
}

/// Validate a topic or service name that may still be relative, private
/// (`~/…`) or carry `{substitution}`s.
///
/// The permissive form: what an application writes at a call site, before
/// [`super::expand`] resolves it against a node.
///
/// # Errors
///
/// Any [`NameFault`].
pub fn validate_unexpanded_name(name: &str) -> Result<(), NameFault> {
    if name.is_empty() {
        return Err(NameFault::Empty);
    }
    for (index, character) in name.char_indices() {
        if !is_name_character(character) {
            return Err(NameFault::IllegalCharacter {
                character,
                offset: index,
            });
        }
    }
    // `~` is only ever the first character, and only ever followed by `/` or
    // by nothing at all: `~foo` is not "the node's foo", it is a typo.
    if let Some(offset) = name
        .char_indices()
        .find_map(|(index, character)| (character == '~' && index != 0).then_some(index))
    {
        return Err(NameFault::MisplacedTilde { offset });
    }
    if name.starts_with('~') {
        let rest = name.get(1..).unwrap_or_default();
        if !rest.is_empty() && !rest.starts_with('/') {
            return Err(NameFault::MisplacedTilde { offset: 1 });
        }
    }
    validate_substitutions(name)?;
    if name.len() > 1 && name.ends_with('/') {
        return Err(NameFault::EmptyToken {
            offset: name.len().saturating_sub(1),
        });
    }

    // Everything after an optional leading `~` and an optional leading `/` is
    // a slash-separated token path; a `{substitution}` stands in for one whole
    // token and is checked by `validate_substitutions` instead.
    let body = name.strip_prefix('~').unwrap_or(name);
    if body.is_empty() {
        return Ok(());
    }
    validate_token_path(body)
}

/// Validate a fully-qualified name: absolute, no `~`, no substitutions.
///
/// What a name has to be by the time it reaches the mangler.
///
/// # Errors
///
/// [`NameFault::NotAbsolute`], [`NameFault::NotFullyQualified`], or whatever
/// [`validate_unexpanded_name`] reports.
pub fn validate_full_name(name: &str) -> Result<(), NameFault> {
    validate_unexpanded_name(name)?;
    if !name.starts_with('/') {
        return Err(NameFault::NotAbsolute);
    }
    if name.contains(['~', '{', '}']) {
        return Err(NameFault::NotFullyQualified);
    }
    Ok(())
}

/// Validate a parameter name: dot-separated tokens, no slashes.
///
/// ROS 2 parameters nest with `.` (`qos_overrides./scan.publisher.depth`),
/// so the separator differs from a topic's, and a leading `/` — which
/// appears inside `qos_overrides.` keys — is accepted inside a token
/// position rather than treated as a path.
///
/// # Errors
///
/// [`NameFault::Empty`] or [`NameFault::IllegalCharacter`].
pub fn validate_parameter_name(name: &str) -> Result<(), NameFault> {
    if name.is_empty() {
        return Err(NameFault::Empty);
    }
    for (index, character) in name.char_indices() {
        if !(is_token_character(character) || matches!(character, '.' | '/')) {
            return Err(NameFault::IllegalCharacter {
                character,
                offset: index,
            });
        }
    }
    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        let offset = name
            .find("..")
            .or_else(|| name.starts_with('.').then_some(0))
            .unwrap_or_else(|| name.len().saturating_sub(1));
        return Err(NameFault::EmptyToken { offset });
    }
    Ok(())
}

/// True when a name is *hidden*: some token begins with `_`.
///
/// `ros2 topic list` omits hidden names unless `--include-hidden-topics` is
/// given; `/_ros2cli_31337/get_parameters` is the shape that convention
/// exists for.
#[must_use]
pub fn is_hidden(name: &str) -> bool {
    name.split('/')
        .any(|token| token.starts_with('_') && token.len() > 1)
}

/// Check every `/`-separated token of a path that already starts with `/` or
/// with a token.
fn validate_token_path(path: &str) -> Result<(), NameFault> {
    let mut offset = 0_usize;
    let body = match path.strip_prefix('/') {
        Some(rest) => {
            offset = 1;
            rest
        }
        None => path,
    };
    for token in body.split('/') {
        if is_substitution(token) {
            offset = offset.saturating_add(token.len()).saturating_add(1);
            continue;
        }
        validate_token(token, offset)?;
        offset = offset.saturating_add(token.len()).saturating_add(1);
    }
    Ok(())
}

/// True when a token is a whole `{substitution}`.
fn is_substitution(token: &str) -> bool {
    token.len() > 2 && token.starts_with('{') && token.ends_with('}')
}

/// Check that every `{` has a matching `}`, that the pair is non-empty, and
/// that the contents are a legal token.
fn validate_substitutions(name: &str) -> Result<(), NameFault> {
    let mut open: Option<usize> = None;
    for (index, character) in name.char_indices() {
        match character {
            '{' => {
                if open.is_some() {
                    return Err(NameFault::MalformedSubstitution { offset: index });
                }
                open = Some(index);
            }
            '}' => {
                let Some(start) = open.take() else {
                    return Err(NameFault::MalformedSubstitution { offset: index });
                };
                let inner = name.get(start.saturating_add(1)..index).unwrap_or_default();
                if inner.is_empty() {
                    return Err(NameFault::MalformedSubstitution { offset: start });
                }
                validate_token(inner, start.saturating_add(1))
                    .map_err(|_| NameFault::MalformedSubstitution { offset: start })?;
            }
            _ => {}
        }
    }
    match open {
        Some(offset) => Err(NameFault::MalformedSubstitution { offset }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_plain_token_is_valid() {
        assert_eq!(validate_token("chatter", 0), Ok(()));
        assert_eq!(validate_token("_hidden", 0), Ok(()));
        assert_eq!(validate_token("a1_b2", 0), Ok(()));
    }

    #[test]
    fn a_token_may_not_start_with_a_digit() {
        assert_eq!(
            validate_token("9lives", 4),
            Err(NameFault::TokenStartsWithDigit { offset: 4 })
        );
    }

    #[test]
    fn an_empty_token_names_its_offset() {
        assert_eq!(
            validate_token("", 7),
            Err(NameFault::EmptyToken { offset: 7 })
        );
    }

    #[test]
    fn an_illegal_character_names_itself_and_its_offset() {
        assert_eq!(
            validate_token("cha tter", 0),
            Err(NameFault::IllegalCharacter {
                character: ' ',
                offset: 3
            })
        );
        assert_eq!(
            validate_token("chatter-2", 10),
            Err(NameFault::IllegalCharacter {
                character: '-',
                offset: 17
            })
        );
    }

    #[test]
    fn node_names_reject_every_path_character() {
        assert_eq!(validate_node_name("talker"), Ok(()));
        assert_eq!(validate_node_name(""), Err(NameFault::Empty));
        assert_eq!(
            validate_node_name("/talker"),
            Err(NameFault::UnexpectedSlash { offset: 0 })
        );
        assert_eq!(
            validate_node_name("~talker"),
            Err(NameFault::MisplacedTilde { offset: 0 })
        );
        assert_eq!(
            validate_node_name("{node}"),
            Err(NameFault::MalformedSubstitution { offset: 0 })
        );
    }

    #[test]
    fn the_root_namespace_is_the_one_bare_slash() {
        assert_eq!(validate_namespace("/"), Ok(()));
        assert_eq!(validate_namespace("/robot"), Ok(()));
        assert_eq!(validate_namespace("/robot/arm"), Ok(()));
        assert_eq!(validate_namespace("robot"), Err(NameFault::NotAbsolute));
        assert_eq!(validate_namespace(""), Err(NameFault::Empty));
        assert!(matches!(
            validate_namespace("/robot/"),
            Err(NameFault::EmptyToken { .. })
        ));
        assert!(matches!(
            validate_namespace("/robot//arm"),
            Err(NameFault::EmptyToken { .. })
        ));
    }

    #[test]
    fn unexpanded_names_accept_relative_private_and_substituted_forms() {
        assert_eq!(validate_unexpanded_name("chatter"), Ok(()));
        assert_eq!(validate_unexpanded_name("/chatter"), Ok(()));
        assert_eq!(validate_unexpanded_name("~/chatter"), Ok(()));
        assert_eq!(validate_unexpanded_name("~"), Ok(()));
        assert_eq!(validate_unexpanded_name("{node}/chatter"), Ok(()));
        assert_eq!(validate_unexpanded_name("/{ns}/chatter"), Ok(()));
    }

    #[test]
    fn a_tilde_anywhere_but_the_start_is_rejected() {
        assert_eq!(
            validate_unexpanded_name("/a/~/b"),
            Err(NameFault::MisplacedTilde { offset: 3 })
        );
        assert_eq!(
            validate_unexpanded_name("~chatter"),
            Err(NameFault::MisplacedTilde { offset: 1 })
        );
    }

    #[test]
    fn an_unbalanced_substitution_is_rejected() {
        assert!(matches!(
            validate_unexpanded_name("/{node/chatter"),
            Err(NameFault::MalformedSubstitution { .. })
        ));
        assert!(matches!(
            validate_unexpanded_name("/node}/chatter"),
            Err(NameFault::MalformedSubstitution { .. })
        ));
        assert!(matches!(
            validate_unexpanded_name("/{}/chatter"),
            Err(NameFault::MalformedSubstitution { .. })
        ));
        assert!(matches!(
            validate_unexpanded_name("/{a{b}}/chatter"),
            Err(NameFault::MalformedSubstitution { .. })
        ));
    }

    #[test]
    fn a_trailing_slash_is_an_empty_token() {
        assert!(matches!(
            validate_unexpanded_name("/chatter/"),
            Err(NameFault::EmptyToken { .. })
        ));
        assert!(matches!(
            validate_unexpanded_name("//chatter"),
            Err(NameFault::EmptyToken { .. })
        ));
    }

    #[test]
    fn full_names_demand_absolute_and_expanded() {
        assert_eq!(validate_full_name("/chatter"), Ok(()));
        assert_eq!(validate_full_name("/robot/arm/state"), Ok(()));
        assert_eq!(validate_full_name("chatter"), Err(NameFault::NotAbsolute));
        assert_eq!(
            validate_full_name("/~/chatter"),
            Err(NameFault::MisplacedTilde { offset: 1 })
        );
        assert_eq!(
            validate_full_name("/{node}/chatter"),
            Err(NameFault::NotFullyQualified)
        );
    }

    #[test]
    fn parameter_names_nest_with_dots() {
        assert_eq!(validate_parameter_name("gain"), Ok(()));
        assert_eq!(
            validate_parameter_name("qos_overrides./scan.publisher.depth"),
            Ok(())
        );
        assert_eq!(validate_parameter_name(""), Err(NameFault::Empty));
        assert!(matches!(
            validate_parameter_name("a..b"),
            Err(NameFault::EmptyToken { .. })
        ));
        assert!(matches!(
            validate_parameter_name(".leading"),
            Err(NameFault::EmptyToken { .. })
        ));
        assert!(matches!(
            validate_parameter_name("trailing."),
            Err(NameFault::EmptyToken { .. })
        ));
        assert!(matches!(
            validate_parameter_name("has space"),
            Err(NameFault::IllegalCharacter { .. })
        ));
    }

    #[test]
    fn hidden_names_are_the_underscore_prefixed_ones() {
        assert!(is_hidden("/_ros2cli_31337/get_parameters"));
        assert!(is_hidden("/robot/_internal"));
        assert!(!is_hidden("/robot/state"));
        assert!(!is_hidden("/"), "a bare slash hides nothing");
        assert!(
            !is_hidden("/robot/_"),
            "a lone underscore is a name, not a marker"
        );
    }

    #[test]
    fn the_character_alphabets_agree_with_each_other() {
        for character in ['a', 'Z', '0', '_'] {
            assert!(is_token_character(character), "{character}");
            assert!(is_name_character(character), "{character}");
        }
        for character in ['/', '~', '{', '}'] {
            assert!(!is_token_character(character), "{character}");
            assert!(is_name_character(character), "{character}");
        }
        for character in [' ', '-', '.', '\0', 'é'] {
            assert!(!is_name_character(character), "{character}");
        }
    }
}
