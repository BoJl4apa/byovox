//! `byovox check`: exercise every stage and say which rung each backend is on.
//! Required stages: config, hotkey mode, backends, microphone, STT. Polish is required only
//! when enabled. Layout and inject are reported, never failed.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::audio::{Audio, SAMPLE_RATE};
use crate::capture::{Capture, CpalCapture, describe_default_device};
use crate::config::{Config, HotkeyConfig, PolishConfig, SttConfig, expand_home, resolve_token};
use crate::hotkey::{HotkeyMode, validate_key_name};
use crate::lang::{LanguagePolicy, SttLanguage};
use crate::polish::{BUILT_IN_PROMPT, PolishClient, Polisher};
use crate::stt::{SttClient, Transcriber};

/// How long the microphone stage records for.
const SAMPLE: Duration = Duration::from_secs(1);

/// Discarded before the peak is measured: WASAPI opens with a click that reads full scale,
/// which would otherwise mask a muted microphone.
const START_TRANSIENT: Duration = Duration::from_millis(150);

/// Less audio than this after the transient is a device that never delivered, not a quiet one.
const MIN_USABLE: Duration = Duration::from_millis(500);

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
/// Control characters become spaces: a `\n` in a reply would wrap the row and a `\r` would
/// rewind the cursor over the `ok`/`FAIL` mark itself.
fn prefix(s: &str) -> String {
    s.chars()
        .take(PREFIX_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A stage error with the server's response body cut off, and nothing else: `check` is the
/// diagnosis surface, so the cause has to survive — a bare `transport` tells nobody anything.
/// Only what `stt.rs`/`polish.rs` append after `HTTP <status>: ` goes, because that is up to
/// 200 characters of raw body, which can be transcript text or a key a 401 echoed back.
fn strip_body(e: &str) -> &str {
    const MARK: &str = " HTTP ";
    let Some(at) = e.find(MARK) else { return e };
    let after = &e[at + MARK.len()..];
    let status = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if status == 0 || !after[status..].starts_with(": ") {
        return e;
    }
    &e[..at + MARK.len() + status]
}

/// The first thing wrong with the `[hotkey]` section, if anything is. The key names are the
/// ones `platform::detect` rejects a row later; validating them here keeps an `ok` mark off a
/// row whose own payload is the invalid value.
fn hotkey_error(h: &HotkeyConfig) -> Option<String> {
    if let Err(e) = validate_key_name(&h.key) {
        return Some(format!("hotkey.key: {e}"));
    }
    if let Err(e) = validate_key_name(&h.cancel_key) {
        return Some(format!("hotkey.cancel_key: {e}"));
    }
    if HotkeyMode::parse(&h.mode).is_none() {
        return Some(format!("hotkey.mode `{}`: expected hold | toggle", h.mode));
    }
    None
}

/// The captured clip minus its start transient: what the peak is measured over and what STT
/// is sent, so the row reports the audio the request actually carried. `Err` when too little
/// arrives to judge — a device that opened but delivered nothing reads as digital silence,
/// and silence is a warning, not the dead-capture failure this command exists to catch.
fn steady_state(a: &Audio) -> Result<Audio, String> {
    let samples_in = |d: Duration| (SAMPLE_RATE as f64 * d.as_secs_f64()) as usize;
    let kept = a.samples.get(samples_in(START_TRANSIENT)..).unwrap_or(&[]);
    if kept.len() < samples_in(MIN_USABLE) {
        return Err(format!(
            "mic delivered {} samples in {:.0} s",
            a.samples.len(),
            SAMPLE.as_secs_f32()
        ));
    }
    Ok(Audio {
        samples: kept.to_vec(),
    })
}

/// The token for a stage, or the message for its FAIL row. An `api_key_env` that names a
/// variable resolving to nothing is a misconfiguration, not an anonymous request: sent to an
/// auth-free endpoint it would answer normally and the stage would print `ok`.
fn stage_token(env_name: &str, file: &str) -> Result<Option<String>, String> {
    let key = resolve_token(env_name, file);
    if key.is_none() && !env_name.is_empty() {
        let from_file = if file.is_empty() {
            ""
        } else {
            " and not found in api_key_file"
        };
        return Err(format!("token: env var {env_name} unset{from_file}"));
    }
    Ok(key)
}

/// `polish.prompt_file` must be readable when set: the daemon reads it at startup, so a path
/// typo has to surface here rather than at the first dictation. `check` only proves it can be
/// read — the round-trip below sends the built-in prompt.
fn prompt_file_error(prompt_file: &str) -> Option<String> {
    if prompt_file.is_empty() {
        return None;
    }
    let path = expand_home(prompt_file);
    std::fs::read_to_string(&path)
        .err()
        .map(|e| format!("polish.prompt_file {}: {e}", path.display()))
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

    // Nothing validates `hotkey.mode` before the daemon runs, and `detect` rejects the key
    // names a row later — too late for this row to have printed them under an `ok`. A bad
    // section fails the check yet stops nothing else: no later stage needs it, and a
    // self-test is worth more when one run reports every problem.
    match hotkey_error(&cfg.hotkey) {
        None => line(
            "hotkey",
            Some(true),
            &format!(
                "{} {}, cancel {}",
                cfg.hotkey.key, cfg.hotkey.mode, cfg.hotkey.cancel_key
            ),
        ),
        Some(e) => {
            line("hotkey", Some(false), &e);
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

    // The token is checked even when there is no clip: it is a fault in the file, and one
    // run should name every fault it can see.
    let stt_row = match stage_token(&cfg.stt.api_key_env, "") {
        Err(e) => Some(Err(e)),
        Ok(key) => audio
            .as_ref()
            .map(|a| stt_round_trip(&cfg.stt, key, a, &language)),
    };
    match stt_row {
        Some(Ok(detail)) => line("stt", Some(true), &detail),
        Some(Err(e)) => {
            line("stt", Some(false), &e);
            all_ok = false;
        }
        None => line("stt", None, "skipped (no usable microphone capture)"),
    }

    if cfg.polish.enabled {
        match polish_round_trip(&cfg.polish) {
            Ok(detail) => line("polish", Some(true), &detail),
            Err(e) => {
                line("polish", Some(false), &e);
                all_ok = false;
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

/// One `SAMPLE`-long recording from the default input device — past its start transient —
/// with the device description alongside. The only place `check` puts a microphone live:
/// `platform::detect` merely reads the device's config, and the `Capture` it hands back is
/// never started.
fn sample_microphone() -> Result<(String, Audio), String> {
    let desc = describe_default_device()?;
    match record().and_then(|a| steady_state(&a)) {
        Ok(a) => Ok((desc, a)),
        // The error already names the rate, channels or format that was refused, or how
        // little audio arrived; the device name says which device it was.
        Err(e) => Err(format!("{desc}: {e}")),
    }
}

/// One transcription of the sampled clip, as the row detail to print. The client error is
/// kept whole apart from its response body, which on a 401 can be the server echoing the key
/// it was presented with.
fn stt_round_trip(
    cfg: &SttConfig,
    key: Option<String>,
    audio: &Audio,
    language: &SttLanguage,
) -> Result<String, String> {
    let client = SttClient::new(
        &cfg.base_url,
        &cfg.model,
        key,
        Duration::from_secs(cfg.timeout_s),
    );
    let prompt = Some(cfg.prompt.as_str()).filter(|p| !p.is_empty());
    let t = Instant::now();
    let text = client
        .transcribe(&audio.to_wav(), language, prompt)
        .map_err(|e| strip_body(&e).to_string())?;
    Ok(format!(
        "{:.2}s  \"{}\"",
        t.elapsed().as_secs_f32(),
        prefix(&text)
    ))
}

/// One polish of a fixed sample dictation, as the row detail to print. Both the token and
/// `prompt_file` are proven before the request, so neither can fail silently behind a
/// gateway that answers anyway. The error loses its body for the same reason as STT's.
fn polish_round_trip(cfg: &PolishConfig) -> Result<String, String> {
    let key = stage_token(&cfg.api_key_env, &cfg.api_key_file)?;
    if let Some(e) = prompt_file_error(&cfg.prompt_file) {
        return Err(e);
    }
    let client = PolishClient::new(
        &cfg.base_url,
        &cfg.model,
        key,
        Duration::from_secs(cfg.timeout_s),
        BUILT_IN_PROMPT.into(),
    );
    let t = Instant::now();
    let text = client
        .polish("um so this is uh a test")
        .map_err(|e| strip_body(&e).to_string())?;
    Ok(format!(
        "{:.2}s  \"{}\"",
        t.elapsed().as_secs_f32(),
        prefix(&text)
    ))
}

fn record() -> Result<Audio, String> {
    let mut cap = CpalCapture::open_default()?;
    cap.start()?;
    std::thread::sleep(SAMPLE);
    cap.stop()
}

#[cfg(test)]
mod tests {
    use super::{
        Audio, HotkeyConfig, PREFIX_CHARS, QUIET_DBFS, SAMPLE_RATE, hotkey_error, prefix,
        prompt_file_error, stage_token, steady_state, strip_body,
    };

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

    /// A reply is server text: a `\n` in it would wrap the row and a `\r` would rewind the
    /// cursor over the mark, so a FAIL could print itself as an `ok`.
    #[test]
    fn a_reply_can_neither_wrap_nor_rewind_its_row() {
        let p = prefix("done\r\nnext\u{7}");
        assert_eq!(p, "done  next ");
        assert!(!p.chars().any(char::is_control));
    }

    /// grok-1: a stage error carries up to 200 characters of response body, and a 401 body
    /// can echo the key it was presented with. The status stays, the body goes.
    #[test]
    fn a_failed_stage_row_never_carries_a_response_body() {
        let e = r#"stt HTTP 401: {"error":"Incorrect API key provided: sk-live-SECRET"}"#;
        assert_eq!(strip_body(e), "stt HTTP 401");
        assert!(!strip_body(e).contains("sk-live-SECRET"));
        assert_eq!(
            strip_body(r#"polish HTTP 404: {"detail":"model not found"}"#),
            "polish HTTP 404"
        );
    }

    /// `check` is the diagnosis surface: an error that carries no response body carries a
    /// cause instead, and cutting at the first colon would throw that away.
    #[test]
    fn an_error_without_a_response_body_reaches_the_row_whole() {
        for e in [
            "stt transport: io: Connection refused",
            "polish transport: io: Connection refused",
            "stt JSON: expected value at line 1 column 1",
            "Microphone Array (48000 Hz, 2 ch, F32): microphone did not start within 5 s",
            "token: env var EXAMPLE_API_KEY unset",
        ] {
            assert_eq!(strip_body(e), e);
        }
    }

    /// The device's opening click reads full scale; measuring the peak over it would report
    /// a muted microphone as a healthy one.
    #[test]
    fn a_start_transient_cannot_mask_a_quiet_microphone() {
        let mut samples = vec![i16::MAX; 2_000];
        samples.resize(SAMPLE_RATE as usize, 100);
        let captured = Audio { samples };
        assert!(captured.peak_dbfs() > -0.1, "{}", captured.peak_dbfs());

        let steady = steady_state(&captured).expect("0.85 s survives the transient");
        assert!(steady.peak_dbfs() < QUIET_DBFS, "{}", steady.peak_dbfs());
        assert!((steady.duration_secs() - 0.85).abs() < 0.01);
    }

    /// A capture that delivered nothing is `-120 dBFS` — indistinguishable from silence, and
    /// silence is only a warning. Too short to judge must FAIL, and say how short.
    #[test]
    fn a_capture_too_short_to_judge_fails_instead_of_reading_as_silence() {
        let empty = Audio { samples: vec![] };
        assert!(empty.peak_dbfs() <= -120.0);
        assert_eq!(
            steady_state(&empty).unwrap_err(),
            "mic delivered 0 samples in 1 s"
        );
        // 300 ms: something arrived, still not enough to judge.
        assert!(
            steady_state(&Audio {
                samples: vec![0; 4_800]
            })
            .is_err()
        );
        // 700 ms leaves 550 ms after the transient, which is enough.
        assert!(
            steady_state(&Audio {
                samples: vec![0; 11_200]
            })
            .is_ok()
        );
    }

    /// A named variable that resolves to nothing is a misconfiguration: against an auth-free
    /// endpoint the unauthenticated request would answer and the stage would print `ok`.
    #[test]
    fn a_named_but_unresolvable_token_fails_instead_of_going_out_anonymous() {
        // `[stt]` has no `api_key_file`, so its message may name only the variable.
        assert_eq!(
            stage_token("BYOVOX_CHECK_NO_SUCH_TOKEN", "").unwrap_err(),
            "token: env var BYOVOX_CHECK_NO_SUCH_TOKEN unset"
        );
        let with_file = stage_token("BYOVOX_CHECK_NO_SUCH_TOKEN", "no-such-dir/env").unwrap_err();
        assert!(
            with_file.ends_with("unset and not found in api_key_file"),
            "{with_file}"
        );
        // An unnamed variable is a deliberate anonymous endpoint, not a fault.
        assert_eq!(stage_token("", "").unwrap(), None);
    }

    /// The daemon reads `polish.prompt_file` at startup; a path typo has to fail the check,
    /// not the first dictation.
    #[test]
    fn an_unreadable_prompt_file_names_the_path_and_the_error() {
        assert_eq!(prompt_file_error(""), None);
        assert_eq!(
            prompt_file_error(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")),
            None
        );
        let e = prompt_file_error("no-such-dir/no-such-prompt.txt").expect("unreadable");
        let (path, io) = e
            .strip_prefix("polish.prompt_file ")
            .and_then(|rest| rest.split_once(": "))
            .expect("`polish.prompt_file <path>: <error>`");
        assert_eq!(path, "no-such-dir/no-such-prompt.txt");
        assert!(!io.is_empty());
    }

    /// `detect` rejects these key names one row later; the hotkey row must not print them
    /// under an `ok` first.
    #[test]
    fn a_key_name_the_daemon_would_reject_fails_the_hotkey_row() {
        assert_eq!(hotkey_error(&HotkeyConfig::default()), None);
        let bad_key = HotkeyConfig {
            key: "Nope".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bad_key)
                .expect("bad key")
                .starts_with("hotkey.key: unknown key `Nope`")
        );
        let bad_cancel = HotkeyConfig {
            cancel_key: "Nope".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bad_cancel)
                .expect("bad cancel key")
                .starts_with("hotkey.cancel_key: unknown key `Nope`")
        );
        let bad_mode = HotkeyConfig {
            mode: "sometimes".into(),
            ..Default::default()
        };
        assert_eq!(
            hotkey_error(&bad_mode).expect("bad mode"),
            "hotkey.mode `sometimes`: expected hold | toggle"
        );
    }
}
