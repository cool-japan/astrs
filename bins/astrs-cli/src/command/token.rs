//! `astrs token mint` (blueprint §16, §22).
//!
//! ```text
//!   astrs token mint --scope mutate   -> the root token itself (back-compat)
//!   astrs token mint --scope read     -> the HKDF-derived read-scope token
//! ```
//!
//! Purely local: unlike every other verb in `command/`, minting never dials
//! a coordinator — it only needs the root secret already on this machine
//! (resolved exactly like every coordinator-facing verb resolves one, via
//! [`crate::runtime_dir::find_token`]), and the scope derivation is a pure
//! function of it ([`astrs_coordinator::auth::derive_read_token`]). A
//! coordinator configured from the same root token always reaches the
//! identical 32 bytes (see that function's own docs), so minting needs
//! neither a network round trip nor a coordinator restart — the credential
//! it prints is already the one a running coordinator will recognise.

use std::io::Write;
use std::path::PathBuf;

use astrs_wire::AuthToken;

use crate::cli::TokenScopeArg;
use crate::error::CliError;
use crate::runtime_dir;

/// `astrs token mint` arguments, independent of `clap`.
#[derive(Debug, Clone)]
pub struct MintArgs {
    /// The scope to mint a credential for.
    pub scope: TokenScopeArg,
    /// An explicit root token, instead of resolving one.
    pub token: Option<String>,
    /// A file holding the root token.
    pub token_file: Option<PathBuf>,
    /// Where `.astrs-token` is looked up when neither of the above is
    /// given.
    pub working_dir: Option<PathBuf>,
    /// Write the minted credential to this file (mode `0600`) instead of
    /// printing it to stdout.
    pub out: Option<PathBuf>,
    /// Emit JSON rather than a bare hex line.
    pub json: bool,
}

/// Mints (derives, or reveals) the credential for one scope from the
/// cluster's root token.
///
/// # Errors
///
/// - [`CliError::NoToken`] if no root token can be found by any of the
///   usual means (§16).
/// - [`CliError::BadToken`] if what was found is not a 64-hex value.
/// - [`CliError::Io`] if `--out` cannot be written.
pub fn mint(out: &mut dyn Write, args: &MintArgs) -> Result<(), CliError> {
    let working_dir = runtime_dir::working_dir(args.working_dir.as_deref());
    let (root, source) = runtime_dir::find_token(
        args.token.as_deref(),
        args.token_file.as_deref(),
        &working_dir,
    )?
    .ok_or(CliError::NoToken {
        env: runtime_dir::ENV_TOKEN,
        file: runtime_dir::TOKEN_FILE,
    })?;

    let minted = match args.scope {
        TokenScopeArg::Mutate => root,
        TokenScopeArg::Read => astrs_coordinator::auth::derive_read_token(&root),
    };

    if let Some(path) = &args.out {
        runtime_dir::write_token_file(path, &minted)?;
    }

    report(out, args, &minted, &source);
    Ok(())
}

/// A stable, lower-case scope name for output — mirrors
/// [`astrs_wire::RequestScope::as_str`], which `TokenScopeArg` sits below in
/// the layer stack and so cannot borrow directly.
const fn scope_name(scope: TokenScopeArg) -> &'static str {
    match scope {
        TokenScopeArg::Read => "read",
        TokenScopeArg::Mutate => "mutate",
    }
}

/// Writes the result, in whichever form `args` asked for.
fn report(
    out: &mut dyn Write,
    args: &MintArgs,
    minted: &AuthToken,
    source: &runtime_dir::TokenSource,
) {
    if args.json {
        emit_json(
            out,
            &serde_json::json!({
                "scope": scope_name(args.scope),
                "token": minted.reveal_hex(),
                "root_source": source.to_string(),
                "written_to": args.out.as_ref().map(|p| p.display().to_string()),
            }),
        );
    } else if let Some(path) = &args.out {
        let _ = writeln!(
            out,
            "{} token written to {}",
            scope_name(args.scope),
            path.display()
        );
        let _ = out.flush();
    } else {
        let _ = writeln!(out, "{}", minted.reveal_hex());
        let _ = out.flush();
    }
}

/// Writes one JSON object.
///
/// Duplicated from `command::record`'s own copy rather than shared — see
/// that module's docs on this crate's small-per-module-helper convention.
fn emit_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
    );
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-token-mint-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(scope: TokenScopeArg, working_dir: &std::path::Path) -> MintArgs {
        MintArgs {
            scope,
            token: None,
            token_file: None,
            working_dir: Some(working_dir.to_path_buf()),
            out: None,
            json: false,
        }
    }

    #[test]
    fn minting_mutate_reveals_the_root_token_unchanged() {
        let dir = scratch("mutate");
        let (root, _) = runtime_dir::ensure_token_file(&dir).unwrap();

        let mut sink = Vec::new();
        mint(&mut sink, &args(TokenScopeArg::Mutate, &dir)).unwrap();
        let printed = String::from_utf8(sink).unwrap();
        assert_eq!(printed.trim(), root.reveal_hex());
    }

    #[test]
    fn minting_read_derives_a_different_value_than_the_root() {
        let dir = scratch("read");
        let (root, _) = runtime_dir::ensure_token_file(&dir).unwrap();

        let mut sink = Vec::new();
        mint(&mut sink, &args(TokenScopeArg::Read, &dir)).unwrap();
        let printed = String::from_utf8(sink).unwrap().trim().to_owned();

        assert_ne!(printed, root.reveal_hex());
        assert_eq!(
            printed,
            astrs_coordinator::auth::derive_read_token(&root).reveal_hex(),
            "the CLI and the coordinator must derive the identical value"
        );
    }

    #[test]
    fn minting_without_a_root_token_is_a_clear_error() {
        let dir = scratch("no-root");
        let error = mint(&mut Vec::new(), &args(TokenScopeArg::Read, &dir)).unwrap_err();
        assert!(matches!(error, CliError::NoToken { .. }), "{error}");
    }

    #[test]
    fn out_writes_the_minted_token_to_a_private_file() {
        let dir = scratch("out-file");
        let (root, _) = runtime_dir::ensure_token_file(&dir).unwrap();
        let out_path = dir.join("read.token");

        let mut a = args(TokenScopeArg::Read, &dir);
        a.out = Some(out_path.clone());
        mint(&mut Vec::new(), &a).unwrap();

        let (found, _) = runtime_dir::find_token(None, Some(&out_path), &dir)
            .unwrap()
            .unwrap();
        assert_eq!(found, astrs_coordinator::auth::derive_read_token(&root));
    }

    #[test]
    fn json_output_names_the_scope_and_token() {
        let dir = scratch("json");
        runtime_dir::ensure_token_file(&dir).unwrap();

        let mut a = args(TokenScopeArg::Read, &dir);
        a.json = true;
        let mut sink = Vec::new();
        mint(&mut sink, &a).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&sink).unwrap();
        assert_eq!(value["scope"], "read");
        assert_eq!(value["token"].as_str().unwrap().len(), 64);
    }
}
