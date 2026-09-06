//! `astrs completion` (blueprint §17): shell completion scripts via
//! `clap_complete`, for bash, zsh and fish.

use std::io::Write;

use crate::error::CliError;

/// A shell `astrs completion` can generate a script for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum CompletionShell {
    /// GNU Bash.
    Bash,
    /// Z shell.
    Zsh,
    /// Fish.
    Fish,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
        }
    }
}

/// Arguments for `astrs completion`.
#[derive(Debug, Clone, Copy)]
pub struct CompletionArgs {
    /// Which shell to generate a script for.
    pub shell: CompletionShell,
}

/// The result of generating one completion script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionReport {
    /// The generated script text — exactly what was written to the
    /// output sink.
    pub script: String,
}

/// Generate a completion script for `command` and print it to `out`.
///
/// `command` is the caller's full clap [`clap::Command`] tree (built from
/// [`crate::cli::Cli::command`]) rather than something this module
/// constructs itself, so a change to the verb tree in `cli.rs` is picked
/// up automatically with no separate description to keep in sync.
///
/// # Errors
///
/// Returns [`CliError::Io`] if writing to `out` fails.
pub fn run(
    out: &mut dyn Write,
    mut command: clap::Command,
    args: &CompletionArgs,
) -> Result<CompletionReport, CliError> {
    let bin_name = command.get_name().to_string();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(
        clap_complete::Shell::from(args.shell),
        &mut command,
        bin_name,
        &mut buf,
    );
    out.write_all(&buf)
        .map_err(|e| CliError::io("<output>", e))?;
    Ok(CompletionReport {
        script: String::from_utf8_lossy(&buf).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn sample_command() -> clap::Command {
        clap::Command::new("astrs")
            .subcommand(clap::Command::new("validate"))
            .subcommand(clap::Command::new("expand"))
    }

    #[test]
    fn bash_completion_mentions_the_binary_name_and_subcommands() {
        let mut out = Vec::new();
        let report = run(
            &mut out,
            sample_command(),
            &CompletionArgs {
                shell: CompletionShell::Bash,
            },
        )
        .unwrap();
        assert!(report.script.contains("astrs"));
        assert!(report.script.contains("validate"));
        assert_eq!(out, report.script.into_bytes());
    }

    #[test]
    fn zsh_and_fish_both_generate_non_empty_scripts() {
        for shell in [CompletionShell::Zsh, CompletionShell::Fish] {
            let mut out = Vec::new();
            let report = run(&mut out, sample_command(), &CompletionArgs { shell }).unwrap();
            assert!(!report.script.is_empty());
        }
    }

    #[test]
    fn shell_value_enum_parses_lowercase_names() {
        use clap::ValueEnum;
        assert_eq!(
            CompletionShell::from_str("bash", true).unwrap(),
            CompletionShell::Bash
        );
        assert_eq!(
            CompletionShell::from_str("fish", true).unwrap(),
            CompletionShell::Fish
        );
    }
}
