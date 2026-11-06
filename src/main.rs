//! itm: an inbox for Herdr task sessions. REQUIREMENTS.md says what it does,
//! DESIGN.md the rules and decisions behind it, PARITY.md how far this Rust
//! version is from the Python one it replaces.

mod cli;
mod daemon;
mod error;
mod herdr;
mod itm;
mod notes;
mod paths;
mod projects;
mod snooze;
mod status;
mod store;
mod switching;
mod system;
mod tasks;
mod times;
mod tui;

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cli::run(cli, itm::Itm::from_env()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("itm: {e}");
            // A Herdr popup closes when its command exits: keep the error readable.
            if std::env::var_os("ITM_POPUP").is_some() {
                std::thread::sleep(Duration::from_millis(2500));
            }
            ExitCode::FAILURE
        }
    }
}
