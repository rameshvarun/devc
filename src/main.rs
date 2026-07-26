//! devc — a simpler alternative to the Dev Containers CLI.
//!
//! Run `devc` inside a project that has a `.devcontainer` spec. devc builds and starts the dev
//! container if it isn't already running, then either drops you into an interactive shell (no
//! arguments) or runs the arguments as a command inside the container (e.g. `devc ./gradlew check`),
//! propagating that command's exit code.

mod config;
mod container;
mod docker;
mod features;

use std::process::ExitCode;

fn main() -> ExitCode {
    // With no arguments, devc opens a shell. Any arguments are the command to run in the container.
    // `-h`/`--help` on its own prints usage; as later arguments it belongs to the inner command
    // (e.g. `devc ./gradlew --help`), so only treat a lone first flag as a usage request.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && (args[0] == "-h" || args[0] == "--help") {
        eprintln!(
            "devc — bring up the current directory's dev container.\n\n\
             Usage:\n  \
             devc                 build/start the container and open an interactive shell\n  \
             devc <command>...    build/start the container and run <command> inside it\n\n\
             Run from a project directory that has a .devcontainer spec."
        );
        return ExitCode::SUCCESS;
    }

    let workspace = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("devc: cannot determine current directory: {e}");
            return ExitCode::FAILURE;
        }
    };

    match container::up_and_run(&workspace, &args) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("devc: {e:#}");
            ExitCode::FAILURE
        }
    }
}
