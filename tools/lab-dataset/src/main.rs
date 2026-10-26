//! `lab-dataset`: checks a lab dataset, or loads it into the ClickHouse the environment names
//! (`lab_dataset::cli::USAGE`).

#![allow(
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "a command line tool: its result and its errors go to the terminal"
)]

use std::process::ExitCode;

use lab_dataset::cli;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(cli::run(command, |name| std::env::var(name).ok())) {
        Ok(output) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
