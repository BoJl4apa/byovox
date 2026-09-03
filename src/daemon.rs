//! The daemon: everything that runs while the tray icon is up.
//!
//! Loads the config, brings logging up, takes the single-instance claim, detects the
//! platform backends, and wires the keyboard hook, the pipeline thread and the IPC server
//! into the winit event loop it then runs on this thread. Depends on nearly every other
//! module; produces a process that lives until `quit`. Both binaries call `run`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::hotkey::HotkeyMode;
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::{capture, capture_log, config, hotkey, ipc, lang, pipeline, platform, polish, stt, ui};

/// How long the process stays up after the UI loop ends, so the IPC connection thread can
/// write the `quit` reply its handler already returned.
const QUIT_GRACE: Duration = Duration::from_millis(200);

/// What the caller of `run` gets to decide.
pub struct Options {
    /// The `--config` the user gave, if any; `None` means the platform default file.
    pub config_path: Option<PathBuf>,
    /// Whether to log to stderr as well as to the file. `byovox run` does; `byovox-daemon`
    /// has no console to log into.
    pub log_to_stderr: bool,
}

/// The windowless daemon binary that lives beside a given executable.
///
/// The pair ship in one directory — `cargo build` and `cargo install` both put them there —
/// and three places cross between them: the CLI spawns the daemon, autostart registers it,
/// and the tray spawns the CLI back for `check`. Resolved from `current_exe` rather than
/// `PATH`, so a build tree and an installed copy can never end up talking to each other.
pub fn daemon_exe(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("byovox-daemon{}", std::env::consts::EXE_SUFFIX))
}

/// The console CLI that lives beside a given executable. The other direction of the pair.
pub fn cli_exe(exe: &Path) -> PathBuf {
    exe.with_file_name(format!("byovox{}", std::env::consts::EXE_SUFFIX))
}

/// Everything the daemon will refuse, decided from the config alone and holding no device.
///
/// The CLI runs this on the console before it spawns anything. The daemon it starts has a null
/// stderr and no log file open until it has read the config, so a config the daemon cannot
/// accept has to be reported by the process the user typed into, or it is reported nowhere.
/// `start` runs the same list again once its log file *is* open — that is the path a Run-key
/// daemon takes, where there is no console at either end.
pub fn preflight(config_path: Option<&Path>) -> Result<Config> {
    let path = config_path.map_or_else(config::default_path, Path::to_path_buf);
    let cfg = config::load(&path)?;
    validate(&cfg)?;
    Ok(cfg)
}

/// The refusals `preflight` and `start` share, so the console and the daemon cannot drift.
fn validate(cfg: &Config) -> Result<()> {
    // Only when `RUST_LOG` is unset: `init_logging` reads the variable first and never looks
    // at the key when it is set, so rejecting the key here would fail a daemon over a line it
    // was not going to read.
    if std::env::var("RUST_LOG").is_err() {
        parse_level(&cfg.logging.level)?;
    }
    HotkeyMode::parse(&cfg.hotkey.mode).ok_or_else(|| {
        anyhow::anyhow!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode)
    })?;
    lang::LanguagePolicy::from_config(&cfg.language)?;
    // A `capture.device` that names nothing must be refused where it can still be read: the
    // daemon this spawns has no console, and the microphone it would otherwise fail to open is
    // not touched until the first dictation. Enumerating names opens no stream, and an empty
    // selector — the default — is not looked up at all.
    capture::validate_selector(&cfg.capture.device).map_err(anyhow::Error::msg)?;
    platform::validate(cfg)?;
    Ok(())
}

/// Owns the appender guard so that every fatal raised once logging is up reaches the log
/// file, not just the caller's `eprintln!` — an autostarted daemon's stderr goes nowhere.
/// `process::exit` runs no destructors, so the guard has to be dropped here by hand.
pub fn run(opts: Options) -> Result<()> {
    let path = opts.config_path.unwrap_or_else(config::default_path);
    let cfg = config::load(&path)?;
    let guard = init_logging(&cfg.logging.level, opts.log_to_stderr)?;
    // Installed only for the daemon, and only once logging is up. The default hook writes the
    // payload to stderr, which the transcript must never reach; the location is what a bug
    // report needs anyway.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(at = panic_location(info.location()), "panicked");
    }));
    if let Err(e) = start(cfg, path) {
        // `{e:#}` to match the caller's rendering: the bare Display would drop the context
        // chain.
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
    tracing::info!(pid = std::process::id(), version = crate::VERSION, config = %path.display(), "byovox starting");

    // The list the spawning CLI already refused on its console, refused again here with the
    // log file open — a daemon the Run key started has no console at either end, and this is
    // the only place its config errors can be recorded. The parses below repeat a few of them
    // because they need the values; they are string comparisons, not devices.
    validate(&cfg)?;
    let mode = HotkeyMode::parse(&cfg.hotkey.mode).ok_or_else(|| {
        anyhow::anyhow!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode)
    })?;
    let policy = lang::LanguagePolicy::from_config(&cfg.language)?;
    if ipc::daemon_running(&ipc::socket_name()) {
        bail!("already running");
    }
    // One line, naming the keys rather than repeating itself per endpoint. `byovox check`
    // says the same thing on the console; this is the copy an autostarted daemon leaves for
    // someone reading the log later, where there was never a console to say it on.
    let mut cleartext: Vec<String> = Vec::new();
    if config::is_cleartext_remote(&cfg.stt.base_url) {
        cleartext.push("stt.base_url".into());
    }
    for (code, lane) in &cfg.stt.by_language {
        if config::is_cleartext_remote(&lane.base_url) {
            cleartext.push(format!("stt.by_language.{code}.base_url"));
        }
    }
    if cfg.polish.enabled && config::is_cleartext_remote(&cfg.polish.base_url) {
        cleartext.push("polish.base_url".into());
    }
    if !cleartext.is_empty() {
        tracing::warn!(keys = %cleartext.join(", "), "{}", config::CLEARTEXT_WARNING);
    }

    let backends = platform::detect(&cfg)?;
    tracing::info!(hotkey = backends.names.hotkey, layout = backends.names.layout, rungs = ?backends.names.rungs, "backends");

    let stt_token = config::resolve_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file);
    let stt_timeout = Duration::from_secs(cfg.stt.timeout_s);
    let stt_scored = cfg.stt.no_speech_threshold > 0.0;
    let mut stt = stt::Routed::new(Box::new(stt::SttClient::new(
        &cfg.stt.base_url,
        &cfg.stt.model,
        stt_token.clone(),
        stt_timeout,
        stt_scored,
    )));
    for (code, lane) in &cfg.stt.by_language {
        // `validate` has already refused any code `Lang::parse` cannot read.
        let Some(lang) = lang::Lang::parse(code) else {
            continue;
        };
        let lane_cfg = cfg.stt.lane_config(lane);
        stt = stt.lane(
            lang,
            Box::new(stt::SttClient::new(
                &lane_cfg.base_url,
                &lane_cfg.model,
                stt_token.clone(),
                stt_timeout,
                stt_scored,
            )),
            Some(lane_cfg.prompt).filter(|p| !p.is_empty()),
        );
        // The key, never the URL: a `base_url` can carry `user:pass@`, and this file is
        // kept for a week. Same choice as the cleartext warning above.
        tracing::info!(lang = %lang, key = %format!("stt.by_language.{code}"), "stt lane");
    }
    let polisher: Option<Box<dyn polish::Polisher>> = if cfg.polish.enabled {
        let base = if cfg.polish.prompt_file.is_empty() {
            polish::built_in(cfg.polish.capitalize_first_word)
        } else {
            // Named in the error: `read_to_string` carries no path, so a typo would
            // otherwise never reveal what `~/…` expanded to.
            let file = config::expand_home(&cfg.polish.prompt_file);
            std::fs::read_to_string(&file)
                .with_context(|| format!("polish.prompt_file {}", file.display()))?
        };
        // The STT glossaries are the polish glossary too: whisper hears the names and writes
        // them in the sentence's script; this stage puts them back in Latin.
        let prompt = polish::prompt_for(&base, &cfg.stt);
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
        Some(Box::new(capture_log::CaptureLog::new(
            dir,
            cfg.capture_log.keep_days,
        )?))
    } else {
        None
    };

    let (event_loop, proxy) = ui::build_event_loop()?;
    let mut pipe = Pipeline::new(
        PipelineConfig {
            initial_mode: mode,
            min_hold: Duration::from_millis(cfg.hotkey.min_hold_ms),
            polish_min_words: cfg.polish.min_words,
            prompt: Some(cfg.stt.prompt.clone()).filter(|p| !p.is_empty()),
            trailing_space: cfg.inject.trailing_space,
            max_chars: cfg.inject.max_chars,
            polish_model: cfg.polish.model.clone(),
            // Narrowed here: the file holds a TOML f64, whisper scores in f32.
            no_speech_threshold: cfg.stt.no_speech_threshold as f32,
            no_speech_warn: cfg.stt.no_speech_warn as f32,
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
                let (forward, reply) = toggle_decision(shared_of(&ipc_shared).enabled);
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
            Request::Status => status_reply(&shared_of(&ipc_shared)),
            Request::Last => match shared_of(&ipc_shared).last_transcript.clone() {
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

/// `Shared` behind a lock a pipeline panic may have poisoned. The IPC handler answers a user
/// who is asking what state the daemon is in; unwinding its connection thread instead of
/// reading the state the pipeline left behind serves nobody. `ipc.rs` already recovers its
/// own handler mutex the same way, and the pipeline posts `Quit` when it dies.
fn shared_of(
    lock: &std::sync::Mutex<pipeline::Shared>,
) -> std::sync::MutexGuard<'_, pipeline::Shared> {
    lock.lock().unwrap_or_else(|p| p.into_inner())
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

/// A directive list into a filter, loudly. `EnvFilter::new` is `parse_lossy`: it drops what
/// it cannot parse and hands back a filter with no directives, so a typo would silently log
/// nothing. `try_new` says so instead.
///
/// This is the whole `RUST_LOG` grammar, on purpose: a bare target (`RUST_LOG=byovox`), a
/// span directive (`RUST_LOG=[my_span]`) and an empty value are all things someone typing an
/// env var means, and none of them is a level.
fn parse_directives(source: &str, value: &str) -> Result<tracing_subscriber::EnvFilter> {
    tracing_subscriber::EnvFilter::try_new(value).with_context(|| format!("{source} `{value}`"))
}

/// `logging.level` on top of that grammar, held to the key's own name.
///
/// A config file is written once and read for months, so its two silent failures are closed:
/// an empty value parses cleanly to a filter with no directives, and a bare word is a valid
/// *target* directive — `try_new("inf")` succeeds, then matches nothing byovox emits. Neither
/// rule may apply to `RUST_LOG`, where both spellings are legitimate.
fn parse_level(value: &str) -> Result<tracing_subscriber::EnvFilter> {
    const SOURCE: &str = "logging.level";
    let value = value.trim();
    if value.is_empty() {
        bail!("{SOURCE} is empty; expected a level like `info` or a directive list");
    }
    if !value.contains(['=', ','])
        && value
            .parse::<tracing_subscriber::filter::LevelFilter>()
            .is_err()
    {
        bail!("{SOURCE} `{value}`: expected trace | debug | info | warn | error | off");
    }
    parse_directives(SOURCE, value)
}

/// The file and line a panic came from, never its payload — a payload can carry transcript
/// text (any `expect` on a string built from one), and the default hook prints it to stderr.
fn panic_location(loc: Option<&std::panic::Location<'_>>) -> String {
    loc.map_or_else(|| "unknown location".to_string(), |l| l.to_string())
}

fn init_logging(
    level: &str,
    to_stderr: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
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
        Ok(v) => parse_directives("RUST_LOG", &v)?,
        Err(_) => parse_level(level)?,
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        // `None` is a no-op layer: the windowless daemon binary has no stderr to write to,
        // and a layer writing into a closed handle is not the same as no layer at all.
        .with(to_stderr.then(|| fmt::layer().with_writer(std::io::stderr)))
        .init();
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cross-binary call resolves the other one this way, so the naming convention is
    /// worth a test that does not need either file to exist.
    #[test]
    fn each_binary_resolves_the_other_beside_itself() {
        let suffix = std::env::consts::EXE_SUFFIX;
        let dir = Path::new("install").join("bin");
        let cli = dir.join(format!("byovox{suffix}"));
        let daemon = dir.join(format!("byovox-daemon{suffix}"));

        assert_eq!(daemon_exe(&cli), daemon);
        assert_eq!(cli_exe(&daemon), cli);
        // Idempotent: `byovox run` is the daemon inside the CLI binary, and the tray's
        // `check` must still find a CLI there.
        assert_eq!(cli_exe(&cli), cli);
        assert_eq!(daemon_exe(&daemon), daemon);
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
        let mut shared = pipeline::Shared::default();
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
        assert!(parse_level("info").is_ok());
        assert!(parse_level("INFO").is_ok());
        assert!(parse_level("byovox=debug,warn").is_ok());

        let err = parse_level("").expect_err("an empty level must be rejected");
        assert!(err.to_string().contains("empty"), "{err}");

        // The realistic typo. `EnvFilter::try_new` accepts it as a *target* named `inf`, so
        // it would parse and then match nothing byovox emits.
        let err = parse_level("inf").expect_err("a typo must be rejected");
        assert!(err.to_string().contains("expected trace"), "{err}");
        assert!(err.to_string().contains("logging.level"), "{err}");
    }

    /// The level shape is a rule about a config key, not about `RUST_LOG`. Someone typing an
    /// env var means the `RUST_LOG` grammar, and a bare target, a span directive and an empty
    /// value are all legitimate there — rejecting them made the standard variable unusable.
    #[test]
    fn rust_log_keeps_the_whole_directive_grammar() {
        for value in ["byovox", "[my_span]", "", "byovox=debug,warn", "off"] {
            assert!(
                parse_directives("RUST_LOG", value).is_ok(),
                "RUST_LOG={value:?} must still work"
            );
        }
        // Genuinely unparseable is still a hard error, and still names the source.
        let err = parse_directives("RUST_LOG", "warn=chatty").expect_err("not a directive list");
        assert!(err.to_string().contains("RUST_LOG"), "{err}");
    }

    /// A panic payload can carry transcript text — any `expect` on a string built from one —
    /// and the default hook prints it to stderr. The location is what a bug report needs.
    #[test]
    #[track_caller]
    fn a_panic_is_logged_by_location_only() {
        let here = panic_location(Some(std::panic::Location::caller()));
        assert!(here.contains("daemon.rs"), "{here}");
        assert!(here.contains(':'), "file:line, {here}");
        assert_eq!(panic_location(None), "unknown location");
    }
}
