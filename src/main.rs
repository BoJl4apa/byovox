//! byovox — push-to-talk dictation against a speech-to-text server you run.
//! CLI entry: subcommand dispatch. The daemon itself lives in `byovox::daemon`.

use byovox::{check, config, daemon, ipc, platform};

use std::path::PathBuf;

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
        None => daemon::run(daemon::Options {
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
        let c = Cli::try_parse_from(["byovox"]).expect("a bare invocation is the daemon");
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
