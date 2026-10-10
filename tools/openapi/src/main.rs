//! `cargo run -p openapi` regenerates `api/openapi.json`; `cargo run -p openapi -- --check`
//! only verifies that the committed file is current and exits with 1 when it is not.

#![allow(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "a command line tool: its result and its errors go to the terminal"
)]

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn spec_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../api/openapi.json")
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let check_only = match (args.next().as_deref(), args.next()) {
        (None, _) => false,
        (Some("--check"), None) => true,
        _ => {
            eprintln!("usage: cargo run -p openapi [-- --check]");
            return ExitCode::from(2);
        }
    };
    match run(check_only) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(check_only: bool) -> Result<String, String> {
    let generated = openapi::generate().map_err(|error| format!("cannot generate: {error}"))?;
    let path = spec_path();
    if check_only {
        let committed = fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        openapi::check(&committed, &generated)?;
        return Ok("api/openapi.json is up to date".to_owned());
    }
    // Write a sibling file and rename it, so an interrupted run never leaves a half-written spec.
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, &generated)
        .and_then(|()| fs::rename(&temporary, &path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(format!(
        "wrote api/openapi.json ({} bytes)",
        generated.len()
    ))
}
