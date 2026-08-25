//! byovox — push-to-talk dictation against a speech-to-text server you run.
//! CLI entry: subcommand dispatch and daemon startup (threads, backends, UI).

use byovox::{capture_log, check, config, hotkey, ipc, lang, pipeline, platform, polish, stt, ui};

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use byovox::config::Config;
use byovox::hotkey::HotkeyMode;
use byovox::pipeline::{Pipeline, PipelineConfig};

/// How long the process stays up after the UI loop ends, so the IPC connection thread can
/// write the `quit` reply its handler already returned.
const QUIT_GRACE: Duration = Duration::from_millis(200);

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
        None => daemon(path),
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

/// Whether to forward a `toggle` into the pipeline, and the reply to send back.
///
/// A disabled pipeline drops the event without a word, so an unconditional `ok` would report
/// success for a dictation that never starts. Split out of the IPC handler so the decision is
/// testable without an event loop or a running daemon.
fn toggle_decision(enabled: bool) -> (bool, ipc::Reply) {
    if enabled {
        (
            true,
            ipc::Reply {
                ok: true,
                ..Default::default()
            },
        )
    } else {
        (
            false,
            ipc::Reply {
                ok: false,
                error: Some("dictation is disabled".into()),
                ..Default::default()
            },
        )
    }
}

/// `enabled` rides along because an idle daemon and a deaf one are otherwise indistinguishable.
fn status_reply(s: &pipeline::Shared) -> ipc::Reply {
    ipc::Reply {
        ok: true,
        state: Some(s.state.into()),
        enabled: Some(s.enabled),
        last_error: s.last_error.clone(),
        ..Default::default()
    }
}

fn log_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// One level or directive list into a filter, loudly.
///
/// There are three ways to end up with a filter that silently logs nothing, and all three are
/// closed here. `EnvFilter::new` is `parse_lossy`: it drops what it cannot parse and hands back
/// a filter with no directives. An empty value parses cleanly to that same nothing. And
/// `try_new` alone is not enough either, because a bare word is a valid *target* directive —
/// `try_new("inf")` succeeds, then matches nothing byovox emits. So a value carrying no
/// directive syntax is held to being what the key is named after: a level.
fn parse_filter(source: &str, value: &str) -> Result<tracing_subscriber::EnvFilter> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{source} is empty; expected a level like `info` or a directive list");
    }
    if !value.contains(['=', ','])
        && value
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_err()
    {
        bail!("{source} `{value}`: expected trace | debug | info | warn | error | off");
    }
    tracing_subscriber::EnvFilter::try_new(value).with_context(|| format!("{source} `{value}`"))
}

fn init_logging(level: &str) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("byovox")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    // `RUST_LOG` is read directly rather than through `try_from_default_env`, whose one `Err`
    // cannot tell "unset" from "invalid": a garbage `RUST_LOG` must fail, not fall through to
    // the config level and trade one silent degradation for another.
    let filter = match std::env::var("RUST_LOG") {
        Ok(v) => parse_filter("RUST_LOG", &v)?,
        Err(_) => parse_filter("logging.level", level)?,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    Ok(guard)
}

/// Owns the appender guard so that every fatal raised once logging is up reaches the log
/// file, not just `main`'s `eprintln!` — an autostarted daemon's stderr goes nowhere.
/// `process::exit` runs no destructors, so the guard has to be dropped here by hand.
fn daemon(path: PathBuf) -> Result<()> {
    let cfg = config::load(&path)?;
    let guard = init_logging(&cfg.logging.level)?;
    if let Err(e) = start(cfg, path) {
        // `{e:#}` to match main's rendering: the bare Display would drop the context chain.
        tracing::error!(
            pid = std::process::id(),
            error = format!("{e:#}"),
            "byovox failed to start"
        );
        drop(guard);
        return Err(e);
    }
    tracing::info!("byovox stopped");
    // Quit arrives over IPC: the handler posts the event and returns its reply, but the
    // connection thread still has to write that line, and the event loop can unwind first.
    // The guard is dropped only after that window, so anything the pipeline logs inside it
    // still reaches the file — `byovox quit` would otherwise see EOF and fail.
    std::thread::sleep(QUIT_GRACE);
    drop(guard);
    std::process::exit(0); // the hook thread blocks in GetMessageW; process exit ends it
}

fn start(cfg: Config, path: PathBuf) -> Result<()> {
    // The pid distinguishes this process from the one already running when the loser of the
    // single-instance check writes its failure into the same daily log file.
    tracing::info!(pid = std::process::id(), version = env!("CARGO_PKG_VERSION"), config = %path.display(), "byovox starting");

    let mode = HotkeyMode::parse(&cfg.hotkey.mode).ok_or_else(|| {
        anyhow::anyhow!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode)
    })?;
    let policy = lang::LanguagePolicy::from_config(&cfg.language)?;
    if ipc::daemon_running(&ipc::socket_name()) {
        bail!("already running");
    }
    let backends = platform::detect(&cfg)?;
    tracing::info!(hotkey = backends.names.hotkey, layout = backends.names.layout, rungs = ?backends.names.rungs, "backends");

    let stt = stt::SttClient::new(
        &cfg.stt.base_url,
        &cfg.stt.model,
        config::resolve_token(&cfg.stt.api_key_env, ""),
        Duration::from_secs(cfg.stt.timeout_s),
    );
    let polisher: Option<Box<dyn polish::Polisher>> = if cfg.polish.enabled {
        let prompt = if cfg.polish.prompt_file.is_empty() {
            polish::BUILT_IN_PROMPT.to_string()
        } else {
            // Named in the error: `read_to_string` carries no path, so a typo would
            // otherwise never reveal what `~/…` expanded to.
            let file = config::expand_home(&cfg.polish.prompt_file);
            std::fs::read_to_string(&file)
                .with_context(|| format!("polish.prompt_file {}", file.display()))?
        };
        let key = config::resolve_token(&cfg.polish.api_key_env, &cfg.polish.api_key_file);
        if key.is_none() && !cfg.polish.api_key_env.is_empty() {
            tracing::warn!(var = %cfg.polish.api_key_env, "polish token not found; polish requests will be unauthenticated");
        }
        Some(Box::new(polish::PolishClient::new(
            &cfg.polish.base_url,
            &cfg.polish.model,
            key,
            Duration::from_secs(cfg.polish.timeout_s),
            prompt,
        )))
    } else {
        None
    };
    let recorder: Option<Box<dyn pipeline::Recorder>> = if cfg.capture_log.enabled {
        let dir = if cfg.capture_log.dir.is_empty() {
            config::data_dir().join("capture")
        } else {
            config::expand_home(&cfg.capture_log.dir)
        };
        Some(Box::new(capture_log::CaptureLog::new(dir)?))
    } else {
        None
    };

    let (event_loop, proxy) = ui::build_event_loop()?;
    let mut pipe = Pipeline::new(
        PipelineConfig {
            mode,
            min_hold: Duration::from_millis(cfg.hotkey.min_hold_ms),
            polish_min_words: cfg.polish.min_words,
            prompt: Some(cfg.stt.prompt.clone()).filter(|p| !p.is_empty()),
            trailing_space: cfg.inject.trailing_space,
            polish_model: cfg.polish.model.clone(),
        },
        backends.capture,
        backends.layout,
        policy,
        Box::new(stt),
        polisher,
        backends.rungs,
        Box::new(ui::ProxyIndicator(proxy.clone())),
        recorder,
    );
    let shared = pipe.shared();

    let (tx, rx) = channel::<hotkey::HotkeyEvent>();
    let backend_tx = tx.clone();
    // The tray's Disable cancels an in-flight recording through the pipeline's own channel.
    let ui_tx = tx.clone();
    let ipc_shared = shared.clone();
    let ipc_proxy = proxy.clone();
    // Bound before the hook goes in. If the single-instance check lost a race, this is where
    // the loser finds out — and at this point it has installed no global keyboard hook and
    // has no pipeline, so two processes never watch the keyboard at once.
    ipc::serve(&ipc::socket_name(), move |req| {
        use ipc::{Reply, Request};
        match req {
            Request::Toggle => {
                // The guard is released at the end of this statement, so nothing is sent
                // while `Shared` is locked.
                let (forward, reply) = toggle_decision(ipc_shared.lock().unwrap().enabled);
                if forward {
                    let _ = tx.send(hotkey::HotkeyEvent::Toggle);
                }
                reply
            }
            Request::Quit => {
                let _ = ipc_proxy.send_event(ui::UserEvent::Quit);
                Reply {
                    ok: true,
                    ..Default::default()
                }
            }
            Request::Status => status_reply(&ipc_shared.lock().unwrap()),
            Request::Last => match ipc_shared.lock().unwrap().last_transcript.clone() {
                Some(t) => Reply {
                    ok: true,
                    text: Some(t),
                    ..Default::default()
                },
                None => Reply {
                    ok: false,
                    error: Some("nothing held yet".into()),
                    ..Default::default()
                },
            },
        }
    })?;

    std::thread::Builder::new()
        .name("byovox-hotkey".into())
        .spawn(move || backends.hotkey.run(backend_tx))?;
    let pipeline_proxy = proxy.clone();
    std::thread::Builder::new()
        .name("byovox-pipeline".into())
        .spawn(move || {
            let run = std::panic::AssertUnwindSafe(|| pipeline::pump(&mut pipe, &rx));
            if std::panic::catch_unwind(run).is_err() {
                tracing::error!("pipeline thread panicked; stopping byovox");
            }
            // Reached only if the pipeline died or every sender dropped. Either way this
            // daemon can no longer dictate, and a tray icon that still says idle is worse
            // than none — so leave through the ordinary Quit path, which flushes the log.
            let _ = pipeline_proxy.send_event(ui::UserEvent::Quit);
        })?;

    ui::run(
        event_loop,
        ui::UiOptions {
            pill: cfg.indicator.pill,
            cue: cfg.indicator.cue,
            version: env!("CARGO_PKG_VERSION"),
        },
        shared,
        ui_tx,
        path,
        log_dir(),
    )
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

    /// The tray's Disable is the only way to reach this state and it is not scriptable, so
    /// the decision is proven here rather than against a running daemon.
    #[test]
    fn toggle_is_refused_while_dictation_is_disabled() {
        let (forward, reply) = toggle_decision(true);
        assert!(forward);
        assert!(reply.ok);
        assert!(reply.error.is_none());

        let (forward, reply) = toggle_decision(false);
        assert!(!forward, "a disabled pipeline must not be sent the event");
        assert!(
            !reply.ok,
            "a dictation that never starts must not report ok"
        );
        assert_eq!(reply.error.as_deref(), Some("dictation is disabled"));
    }

    #[test]
    fn status_reports_enabled_so_idle_and_deaf_are_distinguishable() {
        let mut shared = byovox::pipeline::Shared::default();
        let r = status_reply(&shared);
        assert!(r.ok);
        assert_eq!(r.state.as_deref(), Some("idle"));
        assert_eq!(r.enabled, Some(true));
        assert!(r.last_error.is_none());

        shared.enabled = false;
        shared.last_error = Some("stt".into());
        let r = status_reply(&shared);
        assert_eq!(r.state.as_deref(), Some("idle"), "still idle");
        assert_eq!(r.enabled, Some(false), "but deaf, and it has to say so");
        assert_eq!(r.last_error.as_deref(), Some("stt"));
    }

    #[test]
    fn a_bad_level_is_rejected_rather_than_silently_disabling_logging() {
        assert!(parse_filter("logging.level", "info").is_ok());
        assert!(parse_filter("logging.level", "INFO").is_ok());
        assert!(parse_filter("logging.level", "byovox=debug,warn").is_ok());

        let err = parse_filter("logging.level", "").expect_err("an empty level must be rejected");
        assert!(err.to_string().contains("empty"), "{err}");

        // The realistic typo. `EnvFilter::try_new` accepts it as a *target* named `inf`, so
        // it would parse and then match nothing byovox emits.
        let err = parse_filter("logging.level", "inf").expect_err("a typo must be rejected");
        assert!(err.to_string().contains("expected trace"), "{err}");
        assert!(err.to_string().contains("logging.level"), "{err}");

        let err =
            parse_filter("RUST_LOG", "warn=chatty").expect_err("a bad level must be rejected");
        assert!(err.to_string().contains("RUST_LOG"), "{err}");
    }
}
