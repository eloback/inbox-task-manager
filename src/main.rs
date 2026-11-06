//! itm: an inbox for Herdr task sessions. REQUIREMENTS.md says what it does,
//! DESIGN.md the rules and decisions behind it, PARITY.md how far this Rust
//! version is from the Python one it replaces.

mod cli;
mod config;
mod daemon;
mod error;
mod git;
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
mod workspaces;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match cli::run(cli, itm::Itm::from_env()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error::Error::Exit(status)) => ExitCode::from(status),
        Err(e) => {
            eprintln!("itm: {e}");
            // A Herdr popup closes when its command exits: keep the error readable
            // (a dialog closed on purpose needs no reading).
            if e != error::Error::Cancelled && cli::popup() {
                std::thread::sleep(cli::POPUP_PAUSE);
            }
            ExitCode::FAILURE
        }
    }
}
