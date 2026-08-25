//! `byovox check`: exercise every stage and say which rung each backend is on.
//! Required stages: config, hotkey mode, backends, microphone, STT. Polish is required only
//! when enabled. Layout and inject are reported, never failed.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::audio::Audio;
use crate::capture::{Capture, CpalCapture, describe_default_device};
use crate::config::{Config, resolve_token};
use crate::hotkey::HotkeyMode;
use crate::lang::LanguagePolicy;
use crate::polish::{BUILT_IN_PROMPT, PolishClient, Polisher};
use crate::stt::{SttClient, Transcriber};

/// How long the microphone stage records for.
const SAMPLE: Duration = Duration::from_secs(1);

/// Below this a microphone is muted or attenuated, not merely quiet.
const QUIET_DBFS: f32 = -40.0;

/// How much of a transcript the report may echo.
const PREFIX_CHARS: usize = 60;

fn line(stage: &str, ok: Option<bool>, detail: &str) {
    let mark = match ok {
        Some(true) => "ok  ",
        Some(false) => "FAIL",
        None => "    ",
    };
    println!("{mark} {stage:<9} {detail}");
}

/// The leading characters of a transcript, so no report line ever echoes a whole dictation.
fn prefix(s: &str) -> String {
    s.chars().take(PREFIX_CHARS).collect()
}

pub fn run(cfg: &Config, config_path: &Path) -> bool {
    let mut all_ok = true;

    let source = if config_path.exists() {
        config_path.display().to_string()
    } else {
        "defaults (no file yet — `byovox config --init`)".to_string()
    };
    line("config", Some(true), &source);

    let policy = match LanguagePolicy::from_config(&cfg.language) {
        Ok(p) => p,
        Err(e) => {
            line("language", Some(false), &e.to_string());
            return false;
        }
    };

    // `detect` validates the key names and `inject.mode`, but nothing validates `hotkey.mode`
    // before the daemon runs. A bad mode fails the check yet stops nothing else: no later
    // stage needs it, and a self-test is worth more when one run reports every problem.
    match HotkeyMode::parse(&cfg.hotkey.mode) {
        Some(_) => line(
            "hotkey",
            Some(true),
            &format!(
                "{} {}, cancel {}",
                cfg.hotkey.key, cfg.hotkey.mode, cfg.hotkey.cancel_key
            ),
        ),
        None => {
            line(
                "hotkey",
                Some(false),
                &format!("hotkey.mode `{}`: expected hold | toggle", cfg.hotkey.mode),
            );
            all_ok = false;
        }
    }

    let backends = match crate::platform::detect(cfg) {
        Ok(b) => b,
        Err(e) => {
            line("backends", Some(false), &e.to_string());
            return false;
        }
    };
    line(
        "backends",
        Some(true),
        &format!(
            "hotkey={} layout={} inject={}",
            backends.names.hotkey,
            backends.names.layout,
            backends.names.rungs.join(",")
        ),
    );

    let audio = match sample_microphone() {
        Ok((desc, a)) => {
            let peak = a.peak_dbfs();
            let warn = if peak < QUIET_DBFS {
                "  ← very quiet: muted, or an OS audio enhancement attenuating the mic"
            } else {
                ""
            };
            line(
                "mic",
                Some(true),
                &format!(
                    "{desc}  peak {peak:.1} dBFS over {:.1}s{warn}",
                    a.duration_secs()
                ),
            );
            Some(a)
        }
        Err(e) => {
            line("mic", Some(false), &e);
            all_ok = false;
            None
        }
    };

    let layout = backends.layout.current();
    let language = policy.resolve(layout);
    let fields = language.form_fields();
    let shown = if fields.is_empty() {
        "auto (no language field)".to_string()
    } else {
        fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    line(
        "layout",
        None,
        &format!(
            "{} → {shown}",
            layout
                .map(|l| l.to_string())
                .unwrap_or_else(|| "unreadable".into())
        ),
    );

    let stt_key = resolve_token(&cfg.stt.api_key_env, "");
    let stt = SttClient::new(
        &cfg.stt.base_url,
        &cfg.stt.model,
        stt_key,
        Duration::from_secs(cfg.stt.timeout_s),
    );
    match &audio {
        Some(a) => {
            let prompt = Some(cfg.stt.prompt.as_str()).filter(|p| !p.is_empty());
            let t = Instant::now();
            match stt.transcribe(&a.to_wav(), &language, prompt) {
                Ok(text) => line(
                    "stt",
                    Some(true),
                    &format!("{:.2}s  \"{}\"", t.elapsed().as_secs_f32(), prefix(&text)),
                ),
                Err(e) => {
                    line("stt", Some(false), &e);
                    all_ok = false;
                }
            }
        }
        None => line("stt", None, "skipped (no microphone capture)"),
    }

    if cfg.polish.enabled {
        let key = resolve_token(&cfg.polish.api_key_env, &cfg.polish.api_key_file);
        if key.is_none() && !cfg.polish.api_key_env.is_empty() {
            line(
                "polish",
                Some(false),
                &format!(
                    "token: env var {} unset and not found in api_key_file",
                    cfg.polish.api_key_env
                ),
            );
            all_ok = false;
        } else {
            let client = PolishClient::new(
                &cfg.polish.base_url,
                &cfg.polish.model,
                key,
                Duration::from_secs(cfg.polish.timeout_s),
                BUILT_IN_PROMPT.into(),
            );
            let t = Instant::now();
            match client.polish("um so this is uh a test") {
                Ok(text) => line(
                    "polish",
                    Some(true),
                    &format!("{:.2}s  \"{}\"", t.elapsed().as_secs_f32(), prefix(&text)),
                ),
                Err(e) => {
                    line("polish", Some(false), &e);
                    all_ok = false;
                }
            }
        }
    } else {
        line("polish", None, "disabled");
    }

    line(
        "inject",
        None,
        &format!(
            "dry-run: would use `{}` first",
            backends.names.rungs.first().copied().unwrap_or("none")
        ),
    );
    println!(
        "{}",
        if all_ok {
            "\nall required stages passed"
        } else {
            "\nsome stages FAILED — see above"
        }
    );
    all_ok
}

/// One `SAMPLE`-long recording from the default input device, with the device description
/// alongside. The only place `check` puts a microphone live: `platform::detect` merely reads
/// the device's config, and the `Capture` it hands back is never started.
fn sample_microphone() -> Result<(String, Audio), String> {
    let desc = describe_default_device()?;
    match record() {
        Ok(a) => Ok((desc, a)),
        // The error already names the rate, channels or format that was refused; the device
        // name says which device refused it.
        Err(e) => Err(format!("{desc}: {e}")),
    }
}

fn record() -> Result<Audio, String> {
    let mut cap = CpalCapture::open_default()?;
    cap.start()?;
    std::thread::sleep(SAMPLE);
    cap.stop()
}

#[cfg(test)]
mod tests {
    use super::{PREFIX_CHARS, prefix};

    /// A dictation is private: the report shows a prefix, cut on a character boundary so a
    /// non-ASCII transcript neither panics nor prints more than it promised.
    #[test]
    fn a_transcript_is_reported_only_as_a_short_prefix() {
        let long = "ы".repeat(200);
        let p = prefix(&long);
        assert_eq!(p.chars().count(), PREFIX_CHARS);
        assert!(long.starts_with(&p));
        assert_eq!(prefix("short"), "short");
    }
}
