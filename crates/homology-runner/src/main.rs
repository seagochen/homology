//! `homology-runner`: the runner on its own, with the operations every
//! runner has ([`homology_runner::server::OPS`]).
//!
//! ```text
//! homology-runner serve       one session on stdin/stdout (JSON lines)
//! homology-runner --version   the version and protocol
//! ```

use std::io::{BufReader, Write};
use std::process::ExitCode;

use homology_runner::server::{serve, Ended, Runner};
use homology_runner::{PROTOCOL, VERSION};

/// Run the subcommand the usage above names; anything else is a usage error
/// (exit 64).
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["serve"] => {
            let stdin = std::io::stdin();
            match serve(&Runner::PLAIN, BufReader::new(stdin.lock()), std::io::stdout().lock()) {
                Ok(Ended::Done) => ExitCode::SUCCESS,
                Ok(Ended::Refused(why)) => {
                    let _ = writeln!(std::io::stderr(), "homology-runner: {why}");
                    ExitCode::from(2)
                }
                Err(error) => {
                    let _ = writeln!(std::io::stderr(), "homology-runner: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        ["--version"] => {
            println!("homology-runner {VERSION} (protocol {PROTOCOL})");
            ExitCode::SUCCESS
        }
        _ => {
            let _ = writeln!(std::io::stderr(), "usage: homology-runner serve | --version");
            ExitCode::from(64)
        }
    }
}
