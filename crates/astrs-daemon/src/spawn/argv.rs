//! Argument splitting — shell *rules*, never a shell (§16).
//!
//! > *No shell by default: `build:`/`path:` exec directly (argv split by shlex
//! > rules); `shell:` sources require `--allow-shell` (P1).*
//!
//! The distinction matters more than it looks. `path: ./detect --model $M`
//! must become `["./detect", "--model", "<value of M>"]` — a POSIX word split
//! with quote handling — and it must *not* become a `/bin/sh -c` invocation,
//! because then `; rm -rf ~` in a manifest field would be a command rather
//! than an argument.
//!
//! [`split`] does the word split. What it deliberately does not do is any of
//! the *other* things a shell would: no glob expansion, no `$VAR` (the
//! environment layer already expanded those, against a scrubbed base — see
//! [`crate::spawn::env`]), no `~`, no command substitution, no redirection, no
//! `&&`. A manifest that wants those wants `shell:`, which is a separate,
//! opt-in feature.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::spawn::{split, CommandLine};
//!
//! assert_eq!(split(r#"./detect --model "my model.pt""#)?, ["./detect", "--model", "my model.pt"]);
//!
//! let line = CommandLine::parse("cargo build --release")?;
//! assert_eq!(line.program(), "cargo");
//! assert_eq!(line.args(), ["build", "--release"]);
//! # Ok::<(), astrs_daemon::spawn::ArgvError>(())
//! ```

use std::path::{Path, PathBuf};

/// Why a command line could not be split.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArgvError {
    /// The line has an unterminated quote or a trailing backslash.
    #[error("cannot split {input:?} using shell word rules (unbalanced quote or trailing escape)")]
    Unbalanced {
        /// The offending line.
        input: String,
    },
    /// The line split to nothing at all.
    #[error("command line {input:?} is empty")]
    Empty {
        /// The offending line.
        input: String,
    },
}

/// Splits `input` into words using POSIX shell quoting rules.
///
/// # Errors
///
/// [`ArgvError::Unbalanced`] for an unterminated quote or a trailing
/// backslash. An input that is empty or all whitespace yields an empty vector
/// rather than an error — the emptiness is only a problem when a program name
/// was expected, which is [`CommandLine::parse`]'s business.
///
/// # Examples
///
/// ```
/// use astrs_daemon::spawn::split;
///
/// assert_eq!(split("a  b\tc")?, ["a", "b", "c"]);
/// assert_eq!(split(r"a\ b")?, ["a b"]);
/// assert_eq!(split("'single quoted'")?, ["single quoted"]);
/// assert!(split(r#"unterminated ""#).is_err());
/// assert!(split("   ")?.is_empty());
/// # Ok::<(), astrs_daemon::spawn::ArgvError>(())
/// ```
pub fn split(input: &str) -> Result<Vec<String>, ArgvError> {
    shlex::split(input).ok_or_else(|| ArgvError::Unbalanced {
        input: input.to_string(),
    })
}

/// Splits `input`, refusing an empty result.
///
/// # Errors
///
/// As [`split`], plus [`ArgvError::Empty`] when nothing was there.
pub fn split_non_empty(input: &str) -> Result<Vec<String>, ArgvError> {
    let words = split(input)?;
    if words.is_empty() {
        return Err(ArgvError::Empty {
            input: input.to_string(),
        });
    }
    Ok(words)
}

/// A program and its arguments, ready to hand to a `Command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    /// The program to execute.
    program: String,
    /// Its arguments, in order.
    args: Vec<String>,
}

impl CommandLine {
    /// A command line from an already-split program and arguments.
    ///
    /// This is the manifest's normal shape: `path:` names the program and
    /// `args:` is a list, so nothing needs splitting at all.
    #[must_use]
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// A command line split from one string (a `build:` line, or a `path:`
    /// that carries its own arguments).
    ///
    /// # Errors
    ///
    /// As [`split_non_empty`].
    pub fn parse(input: &str) -> Result<Self, ArgvError> {
        let mut words = split_non_empty(input)?.into_iter();
        let program = words.next().ok_or_else(|| ArgvError::Empty {
            input: input.to_string(),
        })?;
        Ok(Self {
            program,
            args: words.collect(),
        })
    }

    /// A command line from a `path:` that may itself contain arguments, plus
    /// the manifest's own `args:` list appended.
    ///
    /// `path: "python3 -m mynode"` with `args: ["--fast"]` becomes
    /// `python3 -m mynode --fast`. A `path:` naming a file whose own name
    /// contains a space still works, because such a path is quoted in the
    /// manifest and the quote survives the split.
    ///
    /// # Errors
    ///
    /// As [`split_non_empty`].
    pub fn from_path_and_args<I, S>(path: &str, args: I) -> Result<Self, ArgvError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut line = Self::parse(path)?;
        line.args.extend(args.into_iter().map(Into::into));
        Ok(line)
    }

    /// The program to execute.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Its arguments.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Appends an argument.
    pub fn push_arg(&mut self, arg: impl Into<String>) {
        self.args.push(arg.into());
    }

    /// The program, resolved against `working_dir` when it is a relative path
    /// that names a file rather than a `PATH` lookup.
    ///
    /// `./detect` and `bin/detect` resolve against the working directory;
    /// `detect` and `/usr/bin/detect` do not — the first is a `PATH` lookup
    /// the operating system should perform, the second is already absolute.
    /// This mirrors what a shell would do, without being one.
    #[must_use]
    pub fn resolved_program(&self, working_dir: &Path) -> PathBuf {
        let program = Path::new(&self.program);
        if program.is_absolute() || !self.program.contains('/') {
            program.to_path_buf()
        } else {
            working_dir.join(program)
        }
    }

    /// Whether the program will be looked up on `PATH` rather than resolved
    /// against the working directory.
    #[must_use]
    pub fn is_path_lookup(&self) -> bool {
        !self.program.contains('/')
    }

    /// A display form for logs — quoted well enough to read, not to re-parse.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::from(&self.program);
        for arg in &self.args {
            out.push(' ');
            if arg.is_empty() || arg.contains(char::is_whitespace) {
                out.push('"');
                out.push_str(arg);
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

impl core::fmt::Display for CommandLine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn plain_words_split_on_whitespace() {
        assert_eq!(split("a b c").unwrap(), ["a", "b", "c"]);
        assert_eq!(split("a   b\t\tc\n").unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn quotes_group_words() {
        assert_eq!(split(r#""a b" c"#).unwrap(), ["a b", "c"]);
        assert_eq!(split("'a b' c").unwrap(), ["a b", "c"]);
        assert_eq!(split(r#""" x"#).unwrap(), ["", "x"]);
    }

    #[test]
    fn backslashes_escape() {
        assert_eq!(split(r"a\ b").unwrap(), ["a b"]);
        assert_eq!(split(r#""a\"b""#).unwrap(), [r#"a"b"#]);
    }

    #[test]
    fn an_unbalanced_quote_is_refused() {
        for input in [r#"a ""#, "a '", r"a \"] {
            let error = split(input).unwrap_err();
            assert!(matches!(error, ArgvError::Unbalanced { .. }), "{input:?}");
            assert!(error.to_string().contains("shell word rules"));
        }
    }

    #[test]
    fn an_empty_line_splits_to_nothing_but_is_not_a_command() {
        assert!(split("").unwrap().is_empty());
        assert!(split("   \t ").unwrap().is_empty());
        assert!(matches!(
            split_non_empty("  "),
            Err(ArgvError::Empty { .. })
        ));
        assert!(matches!(
            CommandLine::parse(""),
            Err(ArgvError::Empty { .. })
        ));
    }

    #[test]
    fn nothing_is_expanded_that_a_shell_would_expand() {
        // No globbing.
        assert_eq!(split("*.rs").unwrap(), ["*.rs"]);
        // No tilde.
        assert_eq!(split("~/bin/node").unwrap(), ["~/bin/node"]);
        // No variable substitution — the env layer already did that, against
        // a scrubbed base.
        assert_eq!(split("$HOME/bin").unwrap(), ["$HOME/bin"]);
        // No operators: `;` and `&&` are ordinary characters.
        assert_eq!(
            split("run ; rm -rf /").unwrap(),
            ["run", ";", "rm", "-rf", "/"]
        );
        assert_eq!(split("a && b").unwrap(), ["a", "&&", "b"]);
        // No redirection.
        assert_eq!(split("a > b").unwrap(), ["a", ">", "b"]);
    }

    #[test]
    fn a_command_line_parses_program_and_arguments() {
        let line = CommandLine::parse("cargo build --release").unwrap();
        assert_eq!(line.program(), "cargo");
        assert_eq!(line.args(), ["build", "--release"]);
    }

    #[test]
    fn a_path_can_carry_its_own_arguments_before_the_manifest_list() {
        let line = CommandLine::from_path_and_args("python3 -m mynode", ["--fast"]).unwrap();
        assert_eq!(line.program(), "python3");
        assert_eq!(line.args(), ["-m", "mynode", "--fast"]);
    }

    #[test]
    fn a_quoted_path_with_a_space_survives() {
        let line = CommandLine::from_path_and_args(r#""./my node""#, ["--x"]).unwrap();
        assert_eq!(line.program(), "./my node");
        assert_eq!(line.args(), ["--x"]);
    }

    #[test]
    fn an_explicit_program_and_arguments_need_no_splitting() {
        let line = CommandLine::new("./detect", ["--model", "my model.pt"]);
        assert_eq!(line.program(), "./detect");
        assert_eq!(line.args(), ["--model", "my model.pt"]);
    }

    #[test]
    fn arguments_can_be_appended() {
        let mut line = CommandLine::new("./a", Vec::<String>::new());
        line.push_arg("--b");
        assert_eq!(line.args(), ["--b"]);
    }

    #[test]
    fn relative_programs_resolve_against_the_working_directory() {
        let working = Path::new("/workspace");
        assert_eq!(
            CommandLine::new("./detect", Vec::<String>::new()).resolved_program(working),
            PathBuf::from("/workspace/./detect")
        );
        assert_eq!(
            CommandLine::new("bin/detect", Vec::<String>::new()).resolved_program(working),
            PathBuf::from("/workspace/bin/detect")
        );
    }

    #[test]
    fn absolute_and_bare_programs_are_left_alone() {
        let working = Path::new("/workspace");
        assert_eq!(
            CommandLine::new("/usr/bin/env", Vec::<String>::new()).resolved_program(working),
            PathBuf::from("/usr/bin/env")
        );
        assert_eq!(
            CommandLine::new("cargo", Vec::<String>::new()).resolved_program(working),
            PathBuf::from("cargo")
        );
    }

    #[test]
    fn path_lookups_are_recognized() {
        assert!(CommandLine::new("cargo", Vec::<String>::new()).is_path_lookup());
        assert!(!CommandLine::new("./cargo", Vec::<String>::new()).is_path_lookup());
        assert!(!CommandLine::new("/bin/cargo", Vec::<String>::new()).is_path_lookup());
    }

    #[test]
    fn the_display_form_is_readable() {
        let line = CommandLine::new("./detect", ["--model", "my model.pt", ""]);
        assert_eq!(line.to_string(), r#"./detect --model "my model.pt" """#);
    }
}
