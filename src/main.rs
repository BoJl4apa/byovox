//! byovox — push-to-talk dictation against a speech-to-text server you run.
//! CLI entry: subcommand dispatch and daemon startup (threads, backends, UI).

use byovox::{capture_log, check, config, hotkey, ipc, lang, pipeline, platform, polish, stt, ui};

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use byovox::hotkey::HotkeyMode;
use byovox::pipeline::{Pipeline, PipelineConfig};

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
        #[arg(long)]
        init: bool,
    },
    /// Register or unregister per-user autostart
    Autostart {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(config::default_path);
    let result = match cli.cmd {
        None => daemon(path),
        Some(Cmd::Toggle) => relay(ipc::Request::Toggle),
        Some(Cmd::Quit) => relay(ipc::Request::Quit),
        Some(Cmd::Status) => status(),
        Some(Cmd::Last) => last(),
        Some(Cmd::Check { pause }) => check_cmd(path, pause),
        Some(Cmd::Config { init }) => config_cmd(path, init),
        Some(Cmd::Autostart { enable, disable }) => autostart(enable, disable),
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
    println!("state: {}", r.state.unwrap_or_else(|| "?".into()));
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

fn autostart(enable: bool, disable: bool) -> Result<()> {
    #[cfg(windows)]
    {
        if enable {
            platform::windows::autostart::enable()?;
            println!("autostart enabled (HKCU Run key)");
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
        let _ = (enable, disable);
        bail!("autostart on this platform lands with its backends (next plan)")
    }
}

fn log_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "byovox")
        .map(|d| d.data_local_dir().join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

fn init_logging(level: &str) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("byovox")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    Ok(guard)
}

fn daemon(path: PathBuf) -> Result<()> {
    let cfg = config::load(&path)?;
    let guard = init_logging(&cfg.logging.level)?;
    tracing::info!(version = env!("CARGO_PKG_VERSION"), config = %path.display(), "byovox starting");

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
            std::fs::read_to_string(config::expand_home(&cfg.polish.prompt_file))
                .context("polish.prompt_file")?
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
    std::thread::Builder::new()
        .name("byovox-hotkey".into())
        .spawn(move || backends.hotkey.run(backend_tx))?;
    std::thread::Builder::new()
        .name("byovox-pipeline".into())
        .spawn(move || {
            for ev in rx {
                pipe.handle(ev, Instant::now());
            }
        })?;

    // The tray's Disable cancels an in-flight recording through the pipeline's own channel.
    let ui_tx = tx.clone();
    let ipc_shared = shared.clone();
    let ipc_proxy = proxy.clone();
    ipc::serve(&ipc::socket_name(), move |req| {
        use ipc::{Reply, Request};
        match req {
            Request::Toggle => {
                let _ = tx.send(hotkey::HotkeyEvent::Toggle);
                Reply {
                    ok: true,
                    ..Default::default()
                }
            }
            Request::Quit => {
                let _ = ipc_proxy.send_event(ui::UserEvent::Quit);
                Reply {
                    ok: true,
                    ..Default::default()
                }
            }
            Request::Status => {
                let s = ipc_shared.lock().unwrap();
                Reply {
                    ok: true,
                    state: Some(s.state.into()),
                    last_error: s.last_error.clone(),
                    ..Default::default()
                }
            }
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
    )?;
    tracing::info!("byovox stopped");
    // Quit arrives over IPC: the handler posts the event and returns its reply, but the
    // connection thread still has to write that line, and the event loop can unwind first.
    // Drop the appender guard so the last log lines flush, then hold the process open long
    // enough for the reply to land — `byovox quit` would otherwise see EOF and fail.
    drop(guard);
    std::thread::sleep(Duration::from_millis(200));
    std::process::exit(0); // the hook thread blocks in GetMessageW; process exit ends it
}
