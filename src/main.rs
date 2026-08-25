//! byovox — push-to-talk dictation against a speech-to-text server you run.
//! CLI entry: subcommand dispatch. The daemon itself lives in `byovox::daemon`.

use byovox::{check, config, daemon, ipc};
// Only the Windows autostart path names it; on other targets the import would be unused.
#[cfg(windows)]
use byovox::platform;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "byovox",
    version,
    about = "Push-to-talk dictation against a speech-to-text server you run"
)]
struct Cli {
    /// Config file (default: the platform config dir)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in this console, logging to stderr as well as to the file
    Run,
    /// Start or stop recording in the running daemon
    Toggle,
    /// Stop the running daemon
    Quit,
    /// Pipeline state and last error of the running daemon
    Status,
    /// Print the most recent transcript held by the daemon
    Last,
    /// Exercise every stage and report the backend rungs
    Check {
        /// Wait for Enter before exiting (used when launched from the tray)
        #[arg(long)]
        pause: bool,
    },
    /// Print the effective configuration, or write the documented default file
    Config {
        /// Write the documented default file instead of printing the effective config
        #[arg(long)]
        init: bool,
    },
    /// Register or unregister per-user autostart
    Autostart {
        /// Start byovox at login, carrying any --config given here
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Remove the login registration
        #[arg(long)]
        disable: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let given = cli.config.clone();
    let path = cli.config.unwrap_or_else(config::default_path);
    let result = match cli.cmd {
        None => spawn_daemon(given),
        Some(Cmd::Run) => daemon::run(daemon::Options {
            config_path: given,
            log_to_stderr: true,
        }),
        Some(Cmd::Toggle) => relay(ipc::Request::Toggle),
        Some(Cmd::Quit) => relay(ipc::Request::Quit),
        Some(Cmd::Status) => status(),
        Some(Cmd::Last) => last(),
        Some(Cmd::Check { pause }) => check_cmd(path, pause),
        Some(Cmd::Config { init }) => config_cmd(path, init),
        Some(Cmd::Autostart { enable, disable }) => autostart(enable, disable, given),
    };
    if let Err(e) = result {
        eprintln!("byovox: {e:#}");
        std::process::exit(2);
    }
}

/// The bare invocation: start the windowless daemon binary and give the shell its prompt back.
///
/// Detached, with no console inherited from this one and none created, so closing the terminal
/// that typed `byovox` cannot take the tray icon with it. That detachment is also why this
/// function does the two things it does before printing anything: the daemon's stderr goes to
/// `NUL`, so it can neither refuse a config nor die where the user would see it.
fn spawn_daemon(config: Option<PathBuf>) -> Result<()> {
    // Every refusal the daemon would make from the config alone, made on this console first.
    //
    // Before the single-instance check, not after: a broken config is broken whether or not a
    // daemon happens to be up, and "already running" would send the user looking at the wrong
    // thing. This is also the order the daemon itself reads them in.
    daemon::preflight(config.as_deref())?;
    // Answered here rather than by letting the second daemon lose its own single-instance
    // check, because that one loses inside a process with no stderr for anyone to read.
    if ipc::daemon_running(&ipc::socket_name()) {
        bail!("already running");
    }
    let exe = std::env::current_exe().context("current exe")?;
    let daemon_exe = daemon::daemon_exe(&exe);
    if !daemon_exe.exists() {
        bail!(
            "{} not found — the daemon is installed beside this binary",
            daemon_exe.display()
        );
    }
    let mut cmd = std::process::Command::new(&daemon_exe);
    if let Some(c) = &config {
        cmd.arg("--config").arg(c);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x00000008): do not inherit this console, so the daemon never
        // receives its close event. CREATE_NO_WINDOW (0x08000000): nor open one of its own.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", daemon_exe.display()))?;
    // The pid is printed only once the daemon answers its socket, because a pid is what the
    // user will `quit` and `status` against. `try_wait` is what makes the deadline a backstop
    // rather than the usual latency: a daemon that dies is reported the moment it dies, and
    // the deadline is left for one that is alive but has not bound yet — a different failure,
    // which must not claim the process exited.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if ipc::daemon_running(&ipc::socket_name()) {
            break;
        }
        if child
            .try_wait()
            .context("waiting for the daemon")?
            .is_some()
        {
            bail!("daemon exited early — run `byovox run` to see why");
        }
        if std::time::Instant::now() >= deadline {
            bail!("daemon did not answer within 2s — run `byovox run` to see why");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("byovox daemon started (pid {})", child.id());
    Ok(())
}

fn relay(req: ipc::Request) -> Result<()> {
    let reply = ipc::send(&ipc::socket_name(), req).context("is the daemon running?")?;
    if !reply.ok {
        bail!("{}", reply.error.unwrap_or_else(|| "refused".into()));
    }
    Ok(())
}

fn status() -> Result<()> {
    let r =
        ipc::send(&ipc::socket_name(), ipc::Request::Status).context("is the daemon running?")?;
    // A refused reply is a daemon-side failure and must not exit 0 just because it parsed.
    if !r.ok {
        bail!("{}", r.error.unwrap_or_else(|| "refused".into()));
    }
    println!("state: {}", r.state.unwrap_or_else(|| "?".into()));
    if let Some(enabled) = r.enabled {
        println!("enabled: {enabled}");
    }
    if let Some(e) = r.last_error {
        println!("last error: {e}");
    }
    Ok(())
}

fn last() -> Result<()> {
    let r = ipc::send(&ipc::socket_name(), ipc::Request::Last).context("is the daemon running?")?;
    match r.text {
        Some(t) => println!("{t}"),
        None => bail!("{}", r.error.unwrap_or_else(|| "nothing held".into())),
    }
    Ok(())
}

fn check_cmd(path: PathBuf, pause: bool) -> Result<()> {
    let cfg = config::load(&path)?;
    let ok = check::run(&cfg, &path);
    if pause {
        println!("\npress Enter to close");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn config_cmd(path: PathBuf, init: bool) -> Result<()> {
    if init {
        if path.exists() {
            bail!(
                "{} already exists — edit it, or delete it and re-run",
                path.display()
            );
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, config::EXAMPLE)?;
        println!("wrote {}", path.display());
        return Ok(());
    }
    let cfg = config::load(&path)?;
    println!(
        "# effective configuration ({})\n",
        if path.exists() {
            path.display().to_string()
        } else {
            "no file — all defaults".into()
        }
    );
    print!("{}", toml::to_string_pretty(&cfg)?);
    println!("\n# provenance");
    for (key, source) in config::provenance(&path)? {
        println!("{key:<28} {source}");
    }
    Ok(())
}

fn autostart(enable: bool, disable: bool, config: Option<PathBuf>) -> Result<()> {
    #[cfg(windows)]
    {
        if enable {
            platform::windows::autostart::enable(config.as_deref())?;
            match &config {
                Some(c) => println!("autostart enabled (HKCU Run key, --config {})", c.display()),
                None => println!("autostart enabled (HKCU Run key)"),
            }
        } else if disable {
            platform::windows::autostart::disable()?;
            println!("autostart disabled");
        } else {
            bail!("pass --enable or --disable");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (enable, disable, config);
        bail!("autostart on this platform lands with its backends (next plan)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// `conflicts_with` names a field at runtime, so a rename breaks the parser without
    /// breaking the build. `debug_assert` is what catches that.
    #[test]
    fn cli_definition_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_subcommand_is_the_daemon_and_config_is_global() {
        let c = Cli::try_parse_from(["byovox"]).expect("a bare invocation starts the daemon");
        assert!(c.cmd.is_none());
        assert!(c.config.is_none());

        // `global = true` is the one attribute between `--config` working after the
        // subcommand and being rejected.
        let c = Cli::try_parse_from(["byovox", "config", "--config", "x.toml"])
            .expect("--config after the subcommand");
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(c.cmd, Some(Cmd::Config { init: false })));

        let c = Cli::try_parse_from(["byovox", "--config", "x.toml", "status"])
            .expect("--config before the subcommand");
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
        assert!(matches!(c.cmd, Some(Cmd::Status)));

        assert!(matches!(
            Cli::try_parse_from(["byovox", "check", "--pause"])
                .expect("check --pause")
                .cmd,
            Some(Cmd::Check { pause: true })
        ));
    }

    /// The foreground daemon, for watching one start. Same global `--config` as the rest.
    #[test]
    fn run_is_the_foreground_daemon() {
        let c = Cli::try_parse_from(["byovox", "run", "--config", "x.toml"]).expect("run");
        assert!(matches!(c.cmd, Some(Cmd::Run)));
        assert_eq!(c.config.as_deref(), Some(std::path::Path::new("x.toml")));
    }

    #[test]
    fn enable_and_disable_cannot_be_given_together() {
        assert!(Cli::try_parse_from(["byovox", "autostart", "--enable", "--disable"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["byovox", "autostart", "--enable"])
                .expect("--enable alone")
                .cmd,
            Some(Cmd::Autostart {
                enable: true,
                disable: false
            })
        ));
    }
}
