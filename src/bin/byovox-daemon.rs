#![cfg_attr(windows, windows_subsystem = "windows")]
//! The byovox daemon: tray icon, keyboard hook, pipeline, IPC server — in a process with no
//! console of its own.
//!
//! That is the whole reason it exists as a second binary. A console-subsystem process dies
//! with the terminal that launched it and flashes a console window when the Run key starts it
//! at logon; the tray has to outlive the first and never do the second.
//!
//! It carries no CLI beyond `--config`. Everything a person types lives in `byovox`, which
//! spawns this binary — or runs the same `byovox::daemon::run` in the foreground under
//! `byovox run` when you want to watch one start.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "byovox-daemon",
    version,
    about = "The byovox daemon — started by `byovox`, not by hand"
)]
struct Cli {
    /// Config file (default: the platform config dir)
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();
    let result = byovox::daemon::run(byovox::daemon::Options {
        config_path: cli.config,
        log_to_stderr: false,
    });
    if let Err(e) = result {
        // Anything that fails once logging is up is in the log file already. A bad config
        // fails before that, and for a process spawned with a null stderr this line reaches
        // nobody — which is what `byovox run` exists for.
        eprintln!("byovox-daemon: {e:#}");
        std::process::exit(2);
    }
}
