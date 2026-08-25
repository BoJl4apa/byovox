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
    version = byovox::VERSION,
    about = "The byovox daemon — started by `byovox`, not by hand"
)]
struct Cli {
    /// Config file (default: the platform config dir)
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Drop the `NUL` handles the CLI spawned us with, leaving the std handles NULL.
///
/// `Stdio::null()` on Windows is a real open handle to the NUL *device*, so std sets
/// `STARTF_USESTDHANDLES` and every console child we spawn inherits NUL: the tray's
/// `Run check` would get a console window whose `println!`s go to NUL and whose `--pause`
/// reads EOF instantly, so it appears and vanishes. A daemon the Run key starts has NULL
/// handles instead, std then omits `STARTF_USESTDHANDLES`, and the same child is given a
/// console of its own. This makes the two paths identical — the Run-key one is the correct
/// one. `byovox run` keeps its stderr because it never comes through here.
#[cfg(windows)]
fn release_inherited_stdio() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: SetStdHandle only records a handle in the process's own parameters. The
        // handles being replaced are NUL or already NULL, and nothing has been written to
        // them: `Cli::parse` has returned, so `--help` and a parse error have had their say.
        let _ = unsafe { SetStdHandle(id, HANDLE::default()) };
    }
}

fn main() {
    let cli = Cli::parse();
    #[cfg(windows)]
    release_inherited_stdio();
    let result = byovox::daemon::run(byovox::daemon::Options {
        config_path: cli.config,
        log_to_stderr: false,
    });
    if let Err(e) = result {
        // Anything that fails once logging is up is in the log file already, and `byovox`
        // refuses a bad config on its own console before it ever spawns this. Nothing reads
        // this line — stderr is NULL by now — but the exit code is what `try_wait` sees.
        eprintln!("byovox-daemon: {e:#}");
        std::process::exit(2);
    }
}
