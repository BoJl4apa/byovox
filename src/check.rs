//! `byovox check`: exercise every stage and say which rung each backend is on.
//! Required stages: config, hotkey mode, backends, microphone, STT and every STT lane. Polish
//! is required only when enabled. Layout and inject are reported, never failed.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::audio::{Audio, SAMPLE_RATE};
use crate::capture::{Capture, CpalCapture, DeviceInfo, describe_device, input_names};
use crate::config::{
    CLEARTEXT_WARNING, Config, HotkeyConfig, PolishConfig, SttConfig, expand_home,
    is_cleartext_remote, redact_userinfo, resolve_token,
};
use crate::hotkey::{HotkeyMode, parse_chord, validate_key_name};
use crate::lang::{LanguagePolicy, SttLanguage};
use crate::pipeline::no_speech;
use crate::polish::{self, BUILT_IN_PROMPT, PolishClient, Polisher};
use crate::stt::{SttClient, Transcriber};

/// How long the microphone stage records for.
const SAMPLE: Duration = Duration::from_secs(1);

/// Less audio than this is a device that never delivered, not a quiet one.
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

/// A row that is neither a pass nor a failure: something configured is working exactly as
/// asked and is still worth saying out loud. Deliberately not a FAIL — plain HTTP over a
/// private network is a legitimate choice, and `check`'s exit code has to keep meaning
/// "byovox can dictate", or a script that gates on it starts lying.
fn warn_line(stage: &str, detail: &str) {
    println!("warn {stage:<9} {detail}");
}

/// Every configured endpoint that would put its traffic on the wire in clear. Polish is
/// skipped when disabled: the daemon never calls it, so warning about its URL is noise.
fn cleartext_endpoints(cfg: &Config) -> Vec<(String, &str)> {
    let mut out = Vec::new();
    if is_cleartext_remote(&cfg.stt.base_url) {
        out.push(("stt.base_url".to_string(), cfg.stt.base_url.as_str()));
    }
    for (code, lane) in &cfg.stt.by_language {
        if is_cleartext_remote(&lane.base_url) {
            out.push((
                format!("stt.by_language.{code}.base_url"),
                lane.base_url.as_str(),
            ));
        }
    }
    if cfg.polish.enabled && is_cleartext_remote(&cfg.polish.base_url) {
        out.push(("polish.base_url".to_string(), cfg.polish.base_url.as_str()));
    }
    out
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
///
/// Accepted limitation: a 2xx whose JSON simply lacks the `text`/`content` field carries no
/// ` HTTP ` marker, so up to 200 characters of that body reach the row. The clip `check`
/// sends is its own one-second capture and the polish input a fixed sentence, so what can
/// come back is a diagnosis, not a dictation.
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
    if let Err(e) = parse_chord(&h.key) {
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

/// The hotkey row's detail: the key exactly as the file spells it, chord and all, so the row
/// is something the user can compare against the line they edited.
fn hotkey_row(h: &HotkeyConfig) -> String {
    format!("{} {}, cancel {}", h.key, h.mode, h.cancel_key)
}

/// The captured clip, once there is enough of it to judge. The click a WASAPI device opens
/// with is already cut by `CpalCapture::stop`, so what the peak is measured over is what STT
/// is sent. `Err` when too little arrives — a device that opened but delivered nothing reads
/// as digital silence, and silence is a warning, not the dead-capture failure this command
/// exists to catch.
fn enough_audio(a: Audio) -> Result<Audio, String> {
    let min = (SAMPLE_RATE as f64 * MIN_USABLE.as_secs_f64()) as usize;
    if a.samples.len() < min {
        return Err(format!(
            "mic delivered {} usable samples in {:.0} s",
            a.samples.len(),
            SAMPLE.as_secs_f32()
        ));
    }
    Ok(a)
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

/// The base of the system prompt the daemon would send — `polish.prompt_file` when set, else
/// the built-in; `polish::prompt_for` adds the glossary rule on top of it. Reading the file
/// here is what makes a path typo fail the check rather than the first
/// dictation — and it is what the round trip below sends, so `check` exercises the prompt the
/// daemon will use rather than a stand-in that could behave differently.
fn prompt_text(prompt_file: &str) -> Result<String, String> {
    if prompt_file.is_empty() {
        return Ok(BUILT_IN_PROMPT.to_string());
    }
    let path = expand_home(prompt_file);
    std::fs::read_to_string(&path)
        .map_err(|e| format!("polish.prompt_file {}: {e}", path.display()))
}

/// The `warn network` row for every endpoint that would put its traffic on the wire in clear.
///
/// The one place these rows are produced. `byovox setup` deliberately does not call it: the
/// wizard ends by running `check` on the file it just wrote, so printing them itself would say
/// the same thing twice about the same endpoint, seconds apart.
pub fn warn_cleartext(cfg: &Config) {
    for (key, url) in cleartext_endpoints(cfg) {
        // Redacted rather than printed raw: these rows get pasted into bug reports, and a
        // `base_url` can carry `user:pass@`.
        let url = redact_userinfo(url);
        warn_line("network", &format!("{key} {url} is {CLEARTEXT_WARNING}"));
    }
}

/// A second of digital silence: what `byovox setup` probes an STT endpoint with. The wizard
/// asks its questions one at a time and has not reached the microphone, so it cannot offer a
/// real capture — and whether the endpoint answers at all is the whole question at that point.
fn silence() -> Audio {
    Audio {
        samples: vec![0; SAMPLE_RATE as usize],
    }
}

/// The `stt` row, for a `Config` the user is part-way through answering in `byovox setup`:
/// the same token pre-check and the same round trip `run` performs, printed as the same row.
///
/// Takes the whole config, not just `[stt]`, so the language field comes off `[language]`
/// through `LanguagePolicy` — the one encoding `run` uses — rather than being assumed here.
/// The one thing it cannot match is the live keyboard layout: reading that means standing up a
/// platform backend, which the wizard has not done, so this resolves as an unmapped layout
/// does. Identical whenever `language.by_layout` does not name the layout in use, which is the
/// shipped default and always true at the moment the wizard probes.
pub fn probe_stt(cfg: &Config) -> bool {
    let language = match LanguagePolicy::from_config(&cfg.language) {
        Ok(policy) => policy.resolve(None),
        Err(e) => return report("stt", Err(e.to_string())),
    };
    let row = stage_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file)
        .and_then(|key| stt_round_trip(&cfg.stt, key, &silence(), &language));
    report("stt", row)
}

/// The `polish` row, for a gateway the user has just typed into `byovox setup`. As
/// `probe_stt`: `run`'s own stage, printed as `run`'s own row.
pub fn probe_polish(cfg: &Config) -> bool {
    report("polish", polish_round_trip(&cfg.polish, &cfg.stt))
}

/// A stage result as its row, and whether it passed.
fn report(stage: &str, row: Result<String, String>) -> bool {
    match row {
        Ok(detail) => {
            line(stage, Some(true), &detail);
            true
        }
        Err(e) => {
            line(stage, Some(false), &e);
            false
        }
    }
}

pub fn run(cfg: &Config, config_path: &Path) -> bool {
    let mut all_ok = true;

    let source = if config_path.exists() {
        config_path.display().to_string()
    } else {
        "defaults (no file yet — `byovox config --init`)".to_string()
    };
    line("config", Some(true), &source);

    // Near the top, where the endpoints are still the subject: by the time the stt row has
    // printed a happy round trip, "and by the way it was unencrypted" reads as an aside.
    warn_cleartext(cfg);

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
        None => line("hotkey", Some(true), &hotkey_row(&cfg.hotkey)),
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

    // Which microphone before anything is recorded from it, so a `capture.device` that names
    // no device fails as the key it is rather than as a missing microphone.
    let device = describe_device(&cfg.capture.device);
    let audio = match &device {
        Ok(info) => match sample_microphone(&cfg.capture.device, info) {
            Ok(a) => {
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
                        "{info}  peak {peak:.1} dBFS over {:.1}s{warn}",
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
        },
        Err(e) => {
            line("mic", Some(false), e);
            all_ok = false;
            None
        }
    };
    let hands_free = device
        .as_ref()
        .is_ok_and(|info| is_hands_free(&info.name, info.rate));
    if hands_free {
        warn_line("mic", HANDS_FREE_WARNING);
    }
    // The names to choose between, whenever the choice is in question: the row failed, the
    // warning above just asked for a different microphone, or the key is set and this row is
    // what proves which device that spelling actually picked.
    if audio.is_none() || hands_free || !cfg.capture.device.trim().is_empty() {
        line("inputs", None, &input_row());
    }

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
    let stt_row = match stage_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file) {
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
    // One row per language lane, on the lane's endpoint with that language forced — the
    // request the daemon would send for a dictation under that layout.
    for (code, lane) in &cfg.stt.by_language {
        let name = format!("stt[{code}]");
        let lane_cfg = cfg.stt.lane_config(lane);
        let lang = crate::lang::Lang::parse(code).map(SttLanguage::Explicit);
        let row = match (
            stage_token(&cfg.stt.api_key_env, &cfg.stt.api_key_file),
            audio.as_ref(),
            lang,
        ) {
            (Err(e), _, _) => Some(Err(e)),
            (Ok(key), Some(a), Some(l)) => Some(stt_round_trip(&lane_cfg, key, a, &l)),
            _ => None,
        };
        match row {
            Some(Ok(detail)) => line(&name, Some(true), &detail),
            Some(Err(e)) => {
                line(&name, Some(false), &e);
                all_ok = false;
            }
            None => line(&name, None, "skipped (no usable microphone capture)"),
        }
    }

    if cfg.polish.enabled {
        match polish_round_trip(&cfg.polish, &cfg.stt) {
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

/// One `SAMPLE`-long recording from the microphone `capture.device` chose, minus the start
/// click `CpalCapture::stop` cuts. The only place `check` puts a microphone live: `platform::detect` merely reads
/// the device's config, and the `Capture` it hands back is never started.
///
/// The error already names the rate, channels or format that was refused, or how little audio
/// arrived; `info` says which device it was.
fn sample_microphone(selector: &str, info: &DeviceInfo) -> Result<Audio, String> {
    record(selector)
        .and_then(enough_audio)
        .map_err(|e| format!("{info}: {e}"))
}

/// The `inputs` row's detail: every input device name in the order they are enumerated — the
/// order a `capture.device` substring resolves in — or why they could not be listed.
fn input_row() -> String {
    match input_names() {
        Ok(names) if names.is_empty() => "none".to_string(),
        Ok(names) => names.join(" | "),
        Err(e) => e,
    }
}

/// What the `warn mic` row says about a Bluetooth headset's hands-free endpoint.
const HANDS_FREE_WARNING: &str = "this looks like a Bluetooth hands-free profile: dictating through one switches the \
     headset out of stereo for the duration — pin `capture.device` to another microphone";

/// Whether the microphone about to be used is a Bluetooth headset's hands-free endpoint.
/// Windows names it `Headset (… Hands-Free)` and either word is enough on its own; the rate
/// catches a driver that names it something else, because the profile carries 8 or 16 kHz
/// where a microphone worth dictating through reports 44.1 or 48.
fn is_hands_free(name: &str, rate: u32) -> bool {
    let name = name.to_lowercase();
    name.contains("hands-free") || name.contains("headset") || rate <= 16_000
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
        cfg.no_speech_threshold > 0.0,
    );
    let prompt = Some(cfg.prompt.as_str()).filter(|p| !p.is_empty());
    let t = Instant::now();
    let transcript = client
        .transcribe(&audio.to_wav(), language, prompt)
        .map_err(|e| strip_body(&e).to_string())?;
    Ok(format!(
        "{:.2}s  {}\"{}\"",
        t.elapsed().as_secs_f32(),
        no_speech_row(
            transcript.no_speech_prob,
            cfg.no_speech_threshold,
            cfg.no_speech_warn
        ),
        prefix(&transcript.text)
    ))
}

/// What the STT row says about whisper's no-speech score, ready to print before the
/// transcript. `check` records a second of room tone, so the score is usually high and the
/// text beside it usually invented — the row has to show both, or a clean check looks like a
/// hallucinating server. Empty when the server did not score the reply: nothing was measured,
/// and `p_nospeech=0.00` would be a claim.
///
/// The verdict comes from `pipeline::no_speech`, and the configured `f64` is narrowed here
/// exactly as the daemon narrows it: this row's whole job is to say what the daemon would do,
/// so it must not decide a hair's breadth differently.
fn no_speech_row(prob: Option<f32>, threshold: f64, warn: f64) -> String {
    let Some(p) = prob else {
        return String::new();
    };
    let gated = match no_speech(prob, threshold as f32) {
        Some(_) => " (would be dropped as silence)",
        None => match no_speech(prob, warn as f32) {
            Some(_) => " (would play the warning cue)",
            None => "",
        },
    };
    format!("p_nospeech={p:.2}{gated}  ")
}

/// One polish of a fixed sample dictation, as the row detail to print. Both the token and
/// `prompt_file` are proven before the request, so neither can fail silently behind a
/// gateway that answers anyway. The error loses its body for the same reason as STT's.
fn polish_round_trip(cfg: &PolishConfig, stt: &SttConfig) -> Result<String, String> {
    let key = stage_token(&cfg.api_key_env, &cfg.api_key_file)?;
    // The same composition the daemon sends, glossary rule included.
    let prompt = polish::prompt_for(&prompt_text(&cfg.prompt_file)?, stt);
    let client = PolishClient::new(
        &cfg.base_url,
        &cfg.model,
        key,
        Duration::from_secs(cfg.timeout_s),
        prompt,
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

fn record(selector: &str) -> Result<Audio, String> {
    let mut cap = CpalCapture::open(selector)?;
    cap.start()?;
    std::thread::sleep(SAMPLE);
    cap.stop()
}

#[cfg(test)]
mod tests {
    use super::{
        Audio, BUILT_IN_PROMPT, HotkeyConfig, PREFIX_CHARS, QUIET_DBFS, SAMPLE_RATE,
        cleartext_endpoints, enough_audio, hotkey_error, hotkey_row, is_hands_free, no_speech_row,
        prefix, prompt_text, stage_token, strip_body,
    };

    /// Recording through a headset's hands-free endpoint drags it out of stereo for the whole
    /// dictation, which is audible in whatever is playing. Either word in the name says so,
    /// whatever its case, and so does a rate no real microphone reports.
    #[test]
    fn a_bluetooth_hands_free_endpoint_is_warned_about_by_name_or_by_rate() {
        assert!(is_hands_free("Headset (Example Buds Hands-Free)", 16_000));
        assert!(is_hands_free("headset (pamu slide hands-free)", 16_000));
        // Named as a headset but running at a real rate: still the endpoint to avoid.
        assert!(is_hands_free("Headset Microphone (Some Dongle)", 48_000));
        // A driver that says neither word, at a rate that gives it away anyway.
        assert!(is_hands_free("Example Buds (Bluetooth SCO)", 8_000));
        assert!(is_hands_free("Some Microphone", 16_000));

        assert!(!is_hands_free("Microphone Array (Example Audio)", 48_000));
        assert!(!is_hands_free("Microphone (USB Audio Device)", 44_100));
        // The A2DP endpoint of the very same headset is an output, never recorded from; if it
        // ever appears as an input it is not the hands-free profile.
        assert!(!is_hands_free("Headphones (Example Buds Stereo)", 48_000));
    }

    /// `check` transcribes a second of room tone, so a high score beside invented text is the
    /// healthy result — the row has to show the score and say whether the gate would act on
    /// it, or a working setup reads as a hallucinating server. A server that scored nothing
    /// claims nothing.
    #[test]
    fn the_stt_row_says_what_the_no_speech_gate_would_do() {
        assert_eq!(no_speech_row(None, 0.3, 0.08), "");
        assert_eq!(no_speech_row(Some(0.04), 0.3, 0.08), "p_nospeech=0.04  ");
        assert_eq!(
            no_speech_row(Some(0.76), 0.3, 0.08),
            "p_nospeech=0.76 (would be dropped as silence)  "
        );
        // The gray zone: kept, but the daemon would play the warning cue (#26).
        assert_eq!(
            no_speech_row(Some(0.19), 0.3, 0.08),
            "p_nospeech=0.19 (would play the warning cue)  "
        );
        // With the gate off nothing is dropped, however sure whisper is.
        assert_eq!(no_speech_row(Some(0.99), 0.0, 0.0), "p_nospeech=0.99  ");
    }

    /// The row's whole claim is "this is what the daemon would do", so it has to decide on
    /// the same number the daemon does. A score of exactly the threshold as an `f32` sits
    /// *above* the same threshold widened to `f64` — comparing there would have `check`
    /// promise a drop the pipeline does not perform.
    #[test]
    fn the_row_and_the_pipeline_agree_at_the_threshold_itself() {
        let threshold = 0.3_f64;
        let p = threshold as f32;
        assert!(
            f64::from(p) > threshold,
            "the widths must genuinely differ, or this test proves nothing"
        );
        assert_eq!(crate::pipeline::no_speech(Some(p), threshold as f32), None);
        assert_eq!(no_speech_row(Some(p), threshold, 0.0), "p_nospeech=0.30  ");
        // Same knife edge for the warn: exactly at it is in-band, no warning verdict.
        assert_eq!(
            no_speech_row(Some(0.08), 0.3, 0.08_f32 as f64),
            "p_nospeech=0.08  "
        );
    }

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
            // A status arrived, the body did not: the io cause is the whole diagnosis, and
            // `stt.rs` keeps the colon off the status so this survives.
            "stt HTTP 500 body unreadable: io: unexpected end of file",
            "stt JSON: expected value at line 1 column 1",
            "Microphone Array (48000 Hz, 2 ch, F32): microphone did not start within 5 s",
            "token: env var EXAMPLE_API_KEY unset",
        ] {
            assert_eq!(strip_body(e), e);
        }
    }

    /// The click a device opens with is cut before the peak is measured, so click plus room
    /// tone reads as quiet and a muted microphone cannot hide behind it.
    #[test]
    fn a_start_click_cannot_mask_a_quiet_microphone() {
        let mut samples = vec![i16::MAX; 2_000];
        samples.resize(SAMPLE_RATE as usize, 100);
        let steady =
            enough_audio(Audio { samples }.without_start_click()).expect("0.87 s is enough");
        assert!(steady.peak_dbfs() < QUIET_DBFS, "{}", steady.peak_dbfs());
        assert!((steady.duration_secs() - 0.87).abs() < 0.01);
    }

    /// A capture that delivered nothing is `-120 dBFS` — indistinguishable from silence, and
    /// silence is only a warning. Too short to judge must FAIL, and say how short.
    #[test]
    fn a_capture_too_short_to_judge_fails_instead_of_reading_as_silence() {
        let empty = Audio { samples: vec![] };
        assert!(empty.peak_dbfs() <= -120.0);
        assert_eq!(
            enough_audio(empty).unwrap_err(),
            "mic delivered 0 usable samples in 1 s"
        );
        // 300 ms: something arrived, still not enough to judge.
        assert!(
            enough_audio(Audio {
                samples: vec![0; 4_800]
            })
            .is_err()
        );
        // 500 ms is enough.
        assert!(
            enough_audio(Audio {
                samples: vec![0; 8_000]
            })
            .is_ok()
        );
    }

    /// A named variable that resolves to nothing is a misconfiguration: against an auth-free
    /// endpoint the unauthenticated request would answer and the stage would print `ok`.
    #[test]
    fn a_named_but_unresolvable_token_fails_instead_of_going_out_anonymous() {
        // With no `api_key_file` configured the message names only the variable — there is
        // no second place the token could have come from to mention.
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

    /// The daemon reads `polish.prompt_file` at startup and sends what is in it; a path typo
    /// has to fail the check, not the first dictation, and the round trip has to exercise the
    /// configured text rather than the built-in stand-in.
    #[test]
    fn the_configured_prompt_is_what_check_reads_and_sends() {
        assert_eq!(prompt_text("").unwrap(), BUILT_IN_PROMPT);
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let text = prompt_text(manifest).expect("a readable file");
        assert!(
            text.contains("byovox"),
            "the file's own text, not the built-in"
        );
        assert_ne!(text, BUILT_IN_PROMPT);
        let e = prompt_text("no-such-dir/no-such-prompt.txt").unwrap_err();
        let (path, io) = e
            .strip_prefix("polish.prompt_file ")
            .and_then(|rest| rest.split_once(": "))
            .expect("`polish.prompt_file <path>: <error>`");
        assert_eq!(path, "no-such-dir/no-such-prompt.txt");
        assert!(!io.is_empty());
    }

    /// Both endpoints get their own row, and a disabled polish stage gets none: the daemon
    /// never calls it, so warning about a URL it will not use is noise.
    #[test]
    fn every_cleartext_endpoint_that_will_be_used_gets_a_row() {
        use crate::config::{Config, PolishConfig, SttConfig};

        let remote = |url: &str| Config {
            stt: SttConfig {
                base_url: url.into(),
                ..Default::default()
            },
            polish: PolishConfig {
                base_url: url.into(),
                ..Default::default()
            },
            ..Default::default()
        };

        let both = remote("http://10.0.0.5:8770/v1");
        assert_eq!(
            cleartext_endpoints(&both)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url", "polish.base_url"]
        );

        // Polish off: its URL is never contacted, so it must not be reported.
        let mut stt_only = remote("http://10.0.0.5:8770/v1");
        stt_only.polish.enabled = false;
        assert_eq!(
            cleartext_endpoints(&stt_only)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url"]
        );

        // A language lane is one more endpoint, named by its full key.
        let mut with_lane = stt_only.clone();
        with_lane.stt.by_language.insert(
            "he".into(),
            crate::config::SttLane {
                base_url: "http://10.0.0.5:8770/he/v1".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            cleartext_endpoints(&with_lane)
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["stt.base_url", "stt.by_language.he.base_url"]
        );

        // The setup byovox recommends produces no rows at all.
        assert!(cleartext_endpoints(&remote("http://127.0.0.1:8770/v1")).is_empty());
        assert!(cleartext_endpoints(&remote("https://api.example.com/v1")).is_empty());
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

    /// A chord is a `hotkey.key` the daemon accepts, so the row prints it — as configured,
    /// which is what the user has to compare against the file. A bare letter is not.
    #[test]
    fn the_hotkey_row_takes_a_chord_and_still_refuses_a_bare_letter() {
        let chord = HotkeyConfig {
            key: "ControlLeft+ShiftLeft+Z".into(),
            ..Default::default()
        };
        assert_eq!(hotkey_error(&chord), None);
        assert_eq!(
            hotkey_row(&chord),
            "ControlLeft+ShiftLeft+Z hold, cancel Escape"
        );
        assert_eq!(
            hotkey_row(&HotkeyConfig::default()),
            "ControlRight hold, cancel Escape"
        );

        let bare = HotkeyConfig {
            key: "Z".into(),
            ..Default::default()
        };
        assert!(
            hotkey_error(&bare)
                .expect("a bare letter")
                .starts_with("hotkey.key: `Z` needs a modifier")
        );
    }
}
