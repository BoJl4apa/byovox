//! The dictation state machine. Knows no OS: everything platform-specific arrives as a
//! boxed trait. One `handle` call per hotkey event; the return value is the outcome of a
//! finished dictation, if one finished.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::Audio;
use crate::capture::Capture;
use crate::hotkey::{HotkeyEvent, HotkeyMode};
use crate::indicator::{Indicator, IndicatorState};
use crate::inject::Inject;
use crate::lang::{Lang, LanguagePolicy, SttLanguage};
use crate::layout::Layout;
use crate::polish::{Polisher, word_count};
use crate::stt::Transcriber;

pub struct PipelineConfig {
    /// `hotkey.mode` as configured. Only ever read by `Pipeline::new`, which seeds
    /// `Shared::mode` with it — the tray owns the live value from then on.
    pub initial_mode: HotkeyMode,
    pub min_hold: Duration,
    pub polish_min_words: usize,
    pub prompt: Option<String>,
    pub trailing_space: bool,
    /// Discard a transcript whose `no_speech_prob` is above this; `0.0` keeps every one.
    /// `config::load` has already refused anything outside `0.0..=1.0`.
    pub no_speech_threshold: f32,
    /// `polish.model`, recorded on every capture-log row that the polish stage ran for, so
    /// rows from before and after a model change stay distinguishable.
    pub polish_model: String,
}

/// What `byovox status` / `byovox last` read; written by the pipeline thread.
#[derive(Debug)]
pub struct Shared {
    pub state: &'static str,
    /// The tray's Enable/Disable toggle. A disabled pipeline starts nothing; an event
    /// arriving during a recording closes the microphone and discards the audio.
    pub enabled: bool,
    /// The live hotkey mode, seeded from `PipelineConfig::initial_mode` and flipped by the
    /// tray's Mode item. `handle` reads it here rather than from `cfg`, so switching modes
    /// for one long dictation needs no TOML edit and no restart.
    pub mode: HotkeyMode,
    pub last_transcript: Option<String>,
    pub last_error: Option<String>,
}

impl Default for Shared {
    /// A fresh pipeline is idle and listening; `bool::default()` would make it deaf. The
    /// mode is a placeholder: `Pipeline::new` overwrites it with the configured one.
    fn default() -> Shared {
        Shared {
            state: "idle",
            enabled: true,
            mode: HotkeyMode::Hold,
            last_transcript: None,
            last_error: None,
        }
    }
}

/// One finished dictation, for the opt-in capture log.
pub struct DictationRecord<'a> {
    pub audio: &'a Audio,
    /// The keyboard layout that routed this dictation, `None` when it could not be read.
    /// `language` is the policy's answer; only this says what the question was, which is
    /// what a correction rule derived from the corpus has to key on.
    pub layout: Option<Lang>,
    pub language: &'a SttLanguage,
    pub raw: &'a str,
    /// What whisper scored this clip, `None` when the server did not score it. Rows for
    /// dictations the no-speech gate dropped carry it too — that is the corpus this
    /// threshold is tuned from, and without the row the evidence is gone.
    pub no_speech_prob: Option<f32>,
    pub polished: Option<&'a str>,
    /// The polish model, `None` when the stage did not run (disabled, or under min_words).
    pub polish_model: Option<&'a str>,
    pub rung: Option<&'static str>,
    pub stt_ms: u128,
    pub polish_ms: u128,
    pub inject_ms: u128,
}

pub trait Recorder: Send {
    fn record(&mut self, r: &DictationRecord<'_>);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Discarded,
    Empty,
    Inserted {
        rung: &'static str,
    },
    /// Every rung failed; the text is in `Shared::last_transcript`.
    Held,
    /// The microphone could not be closed; no audio, so no request was made.
    CaptureFailed(String),
    SttFailed(String),
}

impl Outcome {
    /// Whether the dictation reached Working — Transcribing/Polishing/Inserting — and so
    /// occupied the pipeline thread long enough for hotkey events to queue up behind it.
    /// `Discarded` (tap, cancel, disabled) and `CaptureFailed` are decided before the first
    /// request, so nothing can have been waiting on them.
    fn reached_working(&self) -> bool {
        !matches!(self, Outcome::Discarded | Outcome::CaptureFailed(_))
    }
}

/// Drive one pipeline from its event channel until every sender is gone.
///
/// The spec is explicit that "a press while Transcribing/Polishing/Inserting is ignored".
/// `handle` runs a whole dictation synchronously, so an event that arrives meanwhile sits in
/// the channel and would be obeyed afterwards — against a state machine that is by then back
/// at Idle, and with a timestamp taken at drain time rather than event time. In toggle mode
/// (including the IPC `toggle` path) that deferred event has nothing to pair with: it opens
/// the microphone and leaves it open until the next toggle or `quit`. So everything queued
/// behind a dictation is dropped, which is what "ignored" means here.
pub fn pump(pipe: &mut Pipeline, rx: &Receiver<HotkeyEvent>) {
    while let Ok(ev) = rx.recv() {
        if pipe
            .handle(ev, Instant::now())
            .is_some_and(|out| out.reached_working())
        {
            while rx.try_recv().is_ok() {}
        }
    }
}

/// The head of a stage error, for WARN and above and for `Shared::last_error`: everything
/// before the first `:`. Stage errors carry a response-body prefix after the colon, which
/// can be transcript text — the full string goes to `debug!`, this summary goes everywhere
/// a human can see it (the log line, the tray menu, `byovox status`).
fn summary(e: &str) -> &str {
    e.split_once(':').map_or(e, |(head, _)| head).trim()
}

/// The score that condemns a transcript as silence, if any: `Some(p)` exactly when this
/// dictation is one the gate drops. A threshold of `0` gates nothing, and a reply the server
/// did not score is never gated.
///
/// The one place the comparison is written. `byovox check` reports what the daemon would do,
/// so it has to decide the same way, on the same `f32` the daemon narrows the configured
/// `f64` to — comparing at two widths makes the two disagree over a band one ulp wide.
pub fn no_speech(prob: Option<f32>, threshold: f32) -> Option<f32> {
    prob.filter(|p| threshold > 0.0 && *p > threshold)
}

enum State {
    Idle,
    Recording {
        since: Instant,
        /// Read at press time, kept beside the language it resolved to: the capture log
        /// records both, and the layout is unreadable again by the time the row is written.
        layout: Option<Lang>,
        language: SttLanguage,
    },
}

pub struct Pipeline {
    pub cfg: PipelineConfig,
    capture: Box<dyn Capture>,
    layout: Box<dyn Layout>,
    policy: LanguagePolicy,
    stt: Box<dyn Transcriber>,
    polish: Option<Box<dyn Polisher>>,
    rungs: Vec<Box<dyn Inject>>,
    indicator: Box<dyn Indicator>,
    recorder: Option<Box<dyn Recorder>>,
    shared: Arc<Mutex<Shared>>,
    state: State,
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: PipelineConfig,
        capture: Box<dyn Capture>,
        layout: Box<dyn Layout>,
        policy: LanguagePolicy,
        stt: Box<dyn Transcriber>,
        polish: Option<Box<dyn Polisher>>,
        rungs: Vec<Box<dyn Inject>>,
        indicator: Box<dyn Indicator>,
        recorder: Option<Box<dyn Recorder>>,
    ) -> Pipeline {
        let shared = Arc::new(Mutex::new(Shared {
            mode: cfg.initial_mode,
            ..Default::default()
        }));
        Pipeline {
            cfg,
            capture,
            layout,
            policy,
            stt,
            polish,
            rungs,
            indicator,
            recorder,
            shared,
            state: State::Idle,
        }
    }

    pub fn shared(&self) -> Arc<Mutex<Shared>> {
        self.shared.clone()
    }

    pub fn handle(&mut self, ev: HotkeyEvent, now: Instant) -> Option<Outcome> {
        let recording = matches!(self.state, State::Recording { .. });
        // Both are the tray's to change, from another thread and possibly mid-dictation, so
        // they are read once per event under one lock.
        let (enabled, mode) = {
            let shared = self.shared.lock().unwrap();
            (shared.enabled, shared.mode)
        };
        // A disabled pipeline is deaf, but it never leaves the microphone open: the first
        // event to arrive during a recording closes it and drops the audio.
        if !enabled {
            if recording {
                let _ = self.capture.stop();
                self.set_state(State::Idle, IndicatorState::Idle);
                tracing::info!("dictation discarded: byovox was disabled mid-recording");
                return Some(Outcome::Discarded);
            }
            return None;
        }
        // In toggle mode a physical press is a toggle and releases are ignored, so a
        // backend that only knows press/release still drives both modes.
        match (ev, mode, recording) {
            (HotkeyEvent::Pressed, HotkeyMode::Hold, false)
            | (HotkeyEvent::Pressed, HotkeyMode::Toggle, false)
            | (HotkeyEvent::Toggle, _, false) => {
                self.begin(now);
                None
            }
            (HotkeyEvent::Released, HotkeyMode::Hold, true)
            | (HotkeyEvent::Pressed, HotkeyMode::Toggle, true)
            | (HotkeyEvent::Toggle, _, true) => Some(self.finish(now)),
            (HotkeyEvent::Cancel, _, true) => {
                let _ = self.capture.stop();
                self.set_state(State::Idle, IndicatorState::Idle);
                tracing::info!("dictation cancelled");
                Some(Outcome::Discarded)
            }
            _ => None,
        }
    }

    fn begin(&mut self, now: Instant) {
        let layout = self.layout.current();
        let language = self.policy.resolve(layout);
        if let Err(e) = self.capture.start() {
            tracing::debug!(error = %e, "microphone open error");
            tracing::error!(error = summary(&e), "microphone open failed");
            self.fail(e);
            return;
        }
        self.set_state(
            State::Recording {
                since: now,
                layout,
                language,
            },
            IndicatorState::Recording,
        );
    }

    fn finish(&mut self, now: Instant) -> Outcome {
        let State::Recording {
            since,
            layout,
            language,
        } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return Outcome::Discarded;
        };
        let audio = match self.capture.stop() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "microphone stop error");
                tracing::error!(error = summary(&e), "microphone stop failed");
                self.fail(e.clone());
                return Outcome::CaptureFailed(e);
            }
        };
        if now.duration_since(since) < self.cfg.min_hold {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!("tap discarded");
            return Outcome::Discarded;
        }
        self.indicator.set(IndicatorState::Working);
        self.shared.lock().unwrap().state = "working";

        // Encoding is byovox's own cost; `stt_ms` reports the server round-trip alone.
        let wav = audio.to_wav();
        let t = Instant::now();
        let transcript = match self
            .stt
            .transcribe(&wav, &language, self.cfg.prompt.as_deref())
        {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "stt error");
                tracing::error!(error = summary(&e), "stt failed");
                self.fail(e.clone());
                return Outcome::SttFailed(e);
            }
        };
        let stt_ms = t.elapsed().as_millis();
        let no_speech_prob = transcript.no_speech_prob;
        let raw = transcript.text;
        tracing::debug!(raw = %raw, "transcript");
        if raw.trim().is_empty() {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!(lang = %language.label(), stt_ms, "empty transcript");
            return Outcome::Empty;
        }
        // Over near-silence whisper invents a stock phrase — "Thank you for watching!" — and
        // reports its own doubt alongside it. The probability decides, never the text: a real
        // one-word utterance must never be dropped for what it happens to say. The row is
        // still written, because these are the captures the threshold is tuned from; nothing
        // else happens, so `byovox last` cannot hand back something never dictated.
        if let Some(p) = no_speech(no_speech_prob, self.cfg.no_speech_threshold) {
            if let Some(rec) = &mut self.recorder {
                rec.record(&DictationRecord {
                    audio: &audio,
                    layout,
                    language: &language,
                    raw: &raw,
                    no_speech_prob,
                    polished: None,
                    polish_model: None,
                    rung: None,
                    stt_ms,
                    polish_ms: 0,
                    inject_ms: 0,
                });
            }
            self.set_state(State::Idle, IndicatorState::Idle);
            // No text on the line: what whisper made up over silence is still a transcript,
            // and transcripts appear at debug only.
            tracing::info!(p = %format!("{p:.2}"), lang = %language.label(), stt_ms, "no speech detected");
            return Outcome::Empty;
        }

        let mut errored = false;
        let polisher = self
            .polish
            .as_ref()
            .filter(|_| word_count(&raw) >= self.cfg.polish_min_words);
        // Named on the row only when the stage actually ran — a failed polish still used the
        // model, a skipped one did not.
        let polish_model = polisher.is_some().then_some(self.cfg.polish_model.as_str());
        let t = Instant::now();
        let polished = match polisher {
            Some(p) => match p.polish(&raw) {
                Ok(text) => Some(text),
                Err(e) => {
                    tracing::debug!(error = %e, "polish error");
                    tracing::warn!(
                        error = summary(&e),
                        "polish failed; inserting raw transcript"
                    );
                    self.shared.lock().unwrap().last_error = Some(summary(&e).to_string());
                    errored = true;
                    None
                }
            },
            None => None,
        };
        let polish_ms = t.elapsed().as_millis();
        tracing::debug!(polished = ?polished, "polished");

        let mut text = polished.clone().unwrap_or_else(|| raw.clone());
        if self.cfg.trailing_space {
            text.push(' ');
        }
        self.shared.lock().unwrap().last_transcript = Some(text.clone());

        let t = Instant::now();
        let mut rung_used = None;
        for rung in &mut self.rungs {
            match rung.inject(&text) {
                Ok(()) => {
                    rung_used = Some(rung.name());
                    break;
                }
                Err(e) => {
                    tracing::debug!(rung = rung.name(), error = %e, "inject rung error");
                    tracing::warn!(
                        rung = rung.name(),
                        error = summary(&e),
                        "inject rung failed"
                    );
                }
            }
        }
        let inject_ms = t.elapsed().as_millis();

        if let Some(rec) = &mut self.recorder {
            rec.record(&DictationRecord {
                audio: &audio,
                layout,
                language: &language,
                raw: &raw,
                no_speech_prob,
                polished: polished.as_deref(),
                polish_model,
                rung: rung_used,
                stt_ms,
                polish_ms,
                inject_ms,
            });
        }

        let total_ms = stt_ms + polish_ms + inject_ms;
        match rung_used {
            Some(rung) => {
                tracing::info!(lang = %language.label(), stt_ms, polish_ms, inject_ms, total_ms, rung, "dictation inserted");
                // The one path that earns the done cue. Every other route back to Idle — a
                // tap, a cancel, a disable, an empty transcript — is silent by spec.
                let final_state = if errored {
                    IndicatorState::Error
                } else {
                    IndicatorState::Done
                };
                self.set_state(State::Idle, final_state);
                Outcome::Inserted { rung }
            }
            None => {
                tracing::warn!(lang = %language.label(), stt_ms, polish_ms, inject_ms, total_ms, "no inject rung worked; transcript held for `byovox last`");
                self.fail("no inject rung worked; run `byovox last`".into());
                Outcome::Held
            }
        }
    }

    fn fail(&mut self, error: String) {
        self.shared.lock().unwrap().last_error = Some(summary(&error).to_string());
        self.set_state(State::Idle, IndicatorState::Error);
    }

    fn set_state(&mut self, state: State, ind: IndicatorState) {
        self.shared.lock().unwrap().state = match state {
            State::Idle => "idle",
            State::Recording { .. } => "recording",
        };
        self.state = state;
        self.indicator.set(ind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguageConfig;
    use crate::hotkey::HotkeyMode;
    use crate::indicator::IndicatorState as S;
    use crate::lang::Lang;
    use crate::stt::Transcript;
    use crate::testutil::fakes::*;
    use std::time::{Duration, Instant};

    struct Rig {
        p: Pipeline,
        cap: FakeCapture,
        stt: FakeTranscriber,
        polish: FakePolisher,
        rung1: FakeInject,
        rung2: FakeInject,
        ind: FakeIndicator,
        rec: FakeRecorder,
    }

    fn rig(
        stt: FakeTranscriber,
        polish: Option<FakePolisher>,
        rung1_fails: bool,
        rung2_fails: bool,
    ) -> Rig {
        rig_with_capture(
            FakeCapture::new(16_000),
            stt,
            polish,
            rung1_fails,
            rung2_fails,
        )
    }

    fn rig_with_capture(
        cap: FakeCapture,
        stt: FakeTranscriber,
        polish: Option<FakePolisher>,
        rung1_fails: bool,
        rung2_fails: bool,
    ) -> Rig {
        let lc = LanguageConfig {
            candidates: vec!["en".into(), "ru".into()],
            by_layout: [("he".to_string(), "he".to_string())].into(),
            ..Default::default()
        };
        let policy = LanguagePolicy::from_config(&lc).unwrap();
        let polish_fake = polish.clone().unwrap_or_else(|| FakePolisher::ok("unused"));
        let rung1 = FakeInject::new("type", rung1_fails);
        let rung2 = FakeInject::new("paste", rung2_fails);
        let ind = FakeIndicator::default();
        let rec = FakeRecorder::default();
        let cfg = PipelineConfig {
            initial_mode: HotkeyMode::Hold,
            min_hold: Duration::from_millis(250),
            polish_min_words: 0,
            prompt: Some("Glossary: Acme".into()),
            trailing_space: false,
            polish_model: "cleanup-1".into(),
            no_speech_threshold: 0.6,
        };
        let p = Pipeline::new(
            cfg,
            Box::new(cap.clone()),
            Box::new(FakeLayout(Lang::parse("he"))),
            policy,
            Box::new(stt.clone()),
            polish.clone().map(|f| Box::new(f) as Box<dyn Polisher>),
            vec![Box::new(rung1.clone()), Box::new(rung2.clone())],
            Box::new(ind.clone()),
            Some(Box::new(rec.clone())),
        );
        Rig {
            p,
            cap,
            stt,
            polish: polish_fake,
            rung1,
            rung2,
            ind,
            rec,
        }
    }

    fn dictate(r: &mut Rig, hold: Duration) -> Option<Outcome> {
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        r.p.handle(HotkeyEvent::Released, t0 + hold)
    }

    #[test]
    fn happy_path_uses_layout_prompt_polish_and_first_rung() {
        let mut r = rig(
            FakeTranscriber::ok("um hello"),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        let out = dictate(&mut r, Duration::from_millis(600));
        assert_eq!(out, Some(Outcome::Inserted { rung: "type" }));
        let calls = r.stt.calls.lock().unwrap();
        assert_eq!(calls[0].0, vec![("language", "he".to_string())]);
        assert_eq!(calls[0].1.as_deref(), Some("Glossary: Acme"));
        assert_eq!(r.polish.calls.lock().unwrap().as_slice(), ["um hello"]);
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["Hello."]);
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert_eq!(
            r.ind.0.lock().unwrap().as_slice(),
            [S::Recording, S::Working, S::Done]
        );
        assert_eq!((r.cap.starts(), r.cap.stops()), (1, 1));
    }

    #[test]
    fn tap_under_min_hold_is_discarded_without_a_request() {
        let mut r = rig(FakeTranscriber::ok("x"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_millis(100)),
            Some(Outcome::Discarded)
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.cap.stops(), 1);
    }

    #[test]
    fn polish_failure_inserts_raw() {
        let mut r = rig(
            FakeTranscriber::ok("raw words"),
            Some(FakePolisher::err("polish HTTP 500: SECRET")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["raw words"]);
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("polish HTTP 500")
        );
    }

    #[test]
    fn stt_failure_inserts_nothing_and_records_error() {
        let mut r = rig(FakeTranscriber::err("boom"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::SttFailed("boom".into()))
        );
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("boom")
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn last_error_keeps_only_the_summary_so_the_tray_cannot_echo_a_response_body() {
        let mut r = rig(
            FakeTranscriber::err("stt HTTP 500: SECRET"),
            None,
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::SttFailed("stt HTTP 500: SECRET".into())),
            "the outcome still carries the whole error for the caller"
        );
        assert_eq!(
            r.p.shared().lock().unwrap().last_error.as_deref(),
            Some("stt HTTP 500")
        );
    }

    #[test]
    fn empty_transcript_is_quietly_idle() {
        let mut r = rig(FakeTranscriber::ok("   "), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));
    }

    /// Spec §Pipeline detail 2 and 5: a sub-`min_hold` tap is discarded *silently* and an
    /// empty transcript gets *no cue* — and a cancelled recording must not sound like a
    /// successful one. `Done` is the only state the UI plays the done tone for, so the claim
    /// is that none of these three ever reaches it.
    #[test]
    fn a_tap_a_cancel_and_an_empty_transcript_are_silent() {
        let mut tap = rig(FakeTranscriber::ok("x"), None, false, false);
        dictate(&mut tap, Duration::from_millis(100));

        let mut cancelled = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        cancelled.p.handle(HotkeyEvent::Pressed, t0);
        cancelled
            .p
            .handle(HotkeyEvent::Cancel, t0 + Duration::from_secs(1));

        let mut empty = rig(FakeTranscriber::ok("   "), None, false, false);
        dictate(&mut empty, Duration::from_secs(1));

        for (what, r) in [("tap", &tap), ("cancel", &cancelled), ("empty", &empty)] {
            let states = r.ind.0.lock().unwrap().clone();
            assert!(
                !states.contains(&S::Done),
                "{what} would play the done cue: {states:?}"
            );
        }
    }

    /// Field finding: over a near-silent hold whisper returns a confident stock phrase and
    /// scores it as silence. Nothing may reach the polisher, the keyboard or `byovox last` —
    /// but the capture row survives, because that corpus is where the threshold came from.
    #[test]
    fn a_transcript_whisper_scored_as_silence_is_dropped() {
        let mut r = rig(
            FakeTranscriber::scored("Thank you for watching!", 0.75),
            Some(FakePolisher::ok("Thank you for watching.")),
            false,
            false,
        );
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Empty)
        );
        assert!(r.polish.calls.lock().unwrap().is_empty());
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert!(r.rung2.texts.lock().unwrap().is_empty());
        assert!(
            r.p.shared().lock().unwrap().last_transcript.is_none(),
            "`byovox last` would hand back something that was never dictated"
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1, "the corpus keeps the evidence");
        assert_eq!(recs[0].raw, "Thank you for watching!");
        assert_eq!(recs[0].no_speech_prob, Some(0.75));
        assert_eq!(recs[0].polished, None);
        assert_eq!(recs[0].rung, None);
    }

    /// The gate has to be invisible to everything that is not silence: a scored transcript
    /// under the threshold, an unscored one from a server that sends no segments, and any
    /// transcript at all once the threshold is 0.
    ///
    /// The first case carries the very words the gated test drops, at a real speech score
    /// against a live threshold. That pair — same text, opposite score — is what "the score
    /// decides, the text does not" means, and it is what a later "let us also blocklist the
    /// stock phrases" change would break.
    #[test]
    fn only_a_score_above_the_threshold_gates() {
        for (what, stt, threshold) in [
            (
                "the stock phrase itself, when whisper scores it as speech",
                FakeTranscriber::scored("Thank you for watching!", 0.04),
                0.6,
            ),
            (
                "an unscored server leaves nothing to judge",
                FakeTranscriber::ok("Thank you for watching!"),
                0.6,
            ),
            (
                "exactly at the threshold is not above it",
                FakeTranscriber::scored("hello there", 0.6),
                0.6,
            ),
            (
                "0 turns the gate off",
                FakeTranscriber::scored("Thank you for watching!", 0.99),
                0.0,
            ),
        ] {
            let mut r = rig(stt, None, false, false);
            r.p.cfg.no_speech_threshold = threshold;
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Inserted { rung: "type" }),
                "{what}"
            );
            assert_eq!(r.rung1.texts.lock().unwrap().len(), 1, "{what}");
        }
    }

    /// The same words, twice, against the same live threshold: dropped at 0.75, inserted at
    /// 0.04. Nothing about the text can be what decided either.
    #[test]
    fn the_same_words_are_dropped_or_inserted_by_their_score_alone() {
        let words = "Thank you for watching!";
        let outcome = |p: f32| {
            let mut r = rig(FakeTranscriber::scored(words, p), None, false, false);
            r.p.cfg.no_speech_threshold = 0.6;
            let out = dictate(&mut r, Duration::from_secs(1));
            (out, r.rung1.texts.lock().unwrap().clone())
        };
        assert_eq!(outcome(0.75), (Some(Outcome::Empty), vec![]));
        assert_eq!(
            outcome(0.04),
            (
                Some(Outcome::Inserted { rung: "type" }),
                vec![words.to_string()]
            )
        );
    }

    thread_local! {
        /// Where this thread's log lines go while `logged` is running, and nowhere when it
        /// is not. Every test thread has its own, so a test running beside `logged` neither
        /// reads its lines nor adds to them.
        static SINK: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    }

    /// The writer behind the one collector this binary installs: a line reaches the buffer of
    /// the thread that emitted it, if that thread armed one, and is dropped otherwise — which
    /// is also what keeps the suite's own output pristine.
    struct ThreadSink;
    impl std::io::Write for ThreadSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            SINK.with(|s| {
                if let Some(armed) = s.borrow_mut().as_mut() {
                    armed.extend_from_slice(buf);
                }
            });
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadSink {
        type Writer = ThreadSink;
        fn make_writer(&'a self) -> ThreadSink {
            ThreadSink
        }
    }

    /// Everything `run` logs at INFO and above, as plain text.
    ///
    /// The collector is installed **globally**, once, rather than scoped to this thread with
    /// `set_default`. `tracing` caches each callsite's interest process-wide, and a scoped
    /// collector loses the race: whichever test thread reaches `tracing::info!` first decides
    /// the cache, and a thread with no collector of its own caches "never" — after which this
    /// helper captures nothing, intermittently and only when the whole suite runs. With one
    /// global collector every thread's interest resolves the same way and the routing is done
    /// by the writer instead.
    fn logged(run: impl FnOnce()) -> String {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| {
            tracing_subscriber::fmt()
                .with_writer(ThreadSink)
                // Colour codes would land between a field name and its value.
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .init();
        });
        SINK.with(|s| *s.borrow_mut() = Some(Vec::new()));
        run();
        let captured = SINK.with(|s| s.borrow_mut().take()).unwrap_or_default();
        String::from_utf8(captured).unwrap()
    }

    /// The gate's line has to say enough to tune the threshold from — the probability that
    /// decided it — and no more: what whisper invents over silence is still a transcript, and
    /// transcripts are a `debug!` thing here, never an `info!` one.
    #[test]
    fn the_gated_line_carries_the_probability_and_no_text() {
        let mut r = rig(
            FakeTranscriber::scored("Thank you for watching!", 0.75),
            None,
            false,
            false,
        );
        let log = logged(|| {
            assert_eq!(
                dictate(&mut r, Duration::from_secs(1)),
                Some(Outcome::Empty)
            );
        });
        assert!(log.contains("no speech detected"), "{log}");
        assert!(log.contains("p=0.75"), "{log}");
        assert!(log.contains("lang=he"), "{log}");
        assert!(!log.contains("Thank you"), "{log}");
    }

    #[test]
    fn first_rung_failing_falls_to_second() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, true, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "paste" })
        );
        assert_eq!(r.rung2.texts.lock().unwrap().as_slice(), ["hi there"]);
    }

    #[test]
    fn all_rungs_failing_holds_for_last() {
        let mut r = rig(FakeTranscriber::ok("hi there"), None, true, true);
        assert_eq!(dictate(&mut r, Duration::from_secs(1)), Some(Outcome::Held));
        assert_eq!(
            r.p.shared().lock().unwrap().last_transcript.as_deref(),
            Some("hi there")
        );
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
    }

    #[test]
    fn min_words_skips_polish() {
        let mut r = rig(
            FakeTranscriber::ok("two words"),
            Some(FakePolisher::ok("X")),
            false,
            false,
        );
        r.p.cfg.polish_min_words = 3;
        dictate(&mut r, Duration::from_secs(1));
        assert!(r.polish.calls.lock().unwrap().is_empty());
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["two words"]);
        // A model that never saw the transcript must not be recorded as having produced it.
        assert_eq!(r.rec.0.lock().unwrap()[0].polish_model, None);
    }

    #[test]
    fn toggle_mode_starts_and_stops_on_toggle() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.shared().lock().unwrap().mode = HotkeyMode::Toggle;
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Toggle, t0).is_none());
        assert!(
            r.p.handle(HotkeyEvent::Released, t0).is_none(),
            "release ignored in toggle mode"
        );
        assert_eq!(
            r.p.handle(HotkeyEvent::Toggle, t0 + Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
    }

    #[test]
    fn cancel_while_recording_discards() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        r.p.handle(HotkeyEvent::Pressed, t0);
        assert_eq!(
            r.p.handle(HotkeyEvent::Cancel, t0 + Duration::from_secs(1)),
            Some(Outcome::Discarded)
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.cap.stops(), 1);
    }

    #[test]
    fn repeat_press_while_recording_is_ignored() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        r.p.handle(HotkeyEvent::Pressed, t0);
        assert!(
            r.p.handle(HotkeyEvent::Pressed, t0 + Duration::from_millis(30))
                .is_none()
        );
        assert_eq!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
    }

    #[test]
    fn trailing_space_is_appended_when_configured() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.cfg.trailing_space = true;
        dictate(&mut r, Duration::from_secs(1));
        assert_eq!(r.rung1.texts.lock().unwrap().as_slice(), ["hi "]);
    }

    #[test]
    fn a_disabled_pipeline_starts_nothing() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.shared().lock().unwrap().enabled = false;
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        assert!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1))
                .is_none()
        );
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!((r.cap.starts(), r.cap.stops()), (0, 0));
        assert!(r.ind.0.lock().unwrap().is_empty());
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
    }

    #[test]
    fn disabling_mid_recording_closes_the_microphone_and_discards_the_audio() {
        let mut r = rig(
            FakeTranscriber::ok("heard while disabled"),
            None,
            false,
            false,
        );
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        r.p.shared().lock().unwrap().enabled = false;
        assert_eq!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1)),
            Some(Outcome::Discarded)
        );
        assert_eq!(r.cap.stops(), 1, "the microphone must not be left open");
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Idle));

        // Re-enabling recovers the next press; the audio captured while disabled is gone.
        r.p.shared().lock().unwrap().enabled = true;
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::Inserted { rung: "type" })
        );
        assert_eq!(r.cap.starts(), 2);
        assert_eq!(r.cap.stops(), 2);
        assert_eq!(r.stt.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn microphone_open_failure_ends_in_error_without_a_request() {
        let mut cap = FakeCapture::new(16_000);
        cap.start_fails = true;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        let t0 = Instant::now();
        assert!(r.p.handle(HotkeyEvent::Pressed, t0).is_none());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
        assert_eq!(r.cap.stops(), 0, "a mic that never opened is never closed");
        assert!(r.stt.calls.lock().unwrap().is_empty());
        // The state machine is back at Idle, so the matching release is a no-op.
        assert!(
            r.p.handle(HotkeyEvent::Released, t0 + Duration::from_secs(1))
                .is_none()
        );
    }

    #[test]
    fn microphone_stop_failure_is_its_own_outcome() {
        let mut cap = FakeCapture::new(16_000);
        cap.stop_fails = true;
        let mut r = rig_with_capture(cap, FakeTranscriber::ok("hi"), None, false, false);
        assert_eq!(
            dictate(&mut r, Duration::from_secs(1)),
            Some(Outcome::CaptureFailed(
                "mic stop: stream ended early".into()
            ))
        );
        assert_eq!(r.cap.stops(), 1);
        assert!(r.stt.calls.lock().unwrap().is_empty());
        assert!(r.rung1.texts.lock().unwrap().is_empty());
        assert_eq!(r.ind.0.lock().unwrap().last(), Some(&S::Error));
        assert_eq!(r.p.shared().lock().unwrap().state, "idle");
    }

    #[test]
    fn the_recorder_sees_the_finished_dictation() {
        let mut r = rig(
            FakeTranscriber::scored("um hello", 0.03),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        dictate(&mut r, Duration::from_secs(1));
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].samples, 16_000);
        // Spec §Pipeline detail 8: the row carries the layout *and* the language it
        // resolved to, plus the model that polished it.
        assert_eq!(recs[0].layout, Lang::parse("he"));
        assert_eq!(recs[0].language, "he");
        assert_eq!(recs[0].raw, "um hello");
        assert_eq!(recs[0].no_speech_prob, Some(0.03));
        assert_eq!(recs[0].polished.as_deref(), Some("Hello."));
        assert_eq!(recs[0].polish_model.as_deref(), Some("cleanup-1"));
        assert_eq!(recs[0].rung, Some("type"));
        // Wall-clock timings: only a sanity bound is non-flaky, but it pins them as wired.
        for ms in [recs[0].stt_ms, recs[0].polish_ms, recs[0].inject_ms] {
            assert!(
                ms < 60_000,
                "{ms} ms is not a plausible fake-backend timing"
            );
        }
    }

    /// A transcriber that posts a hotkey event into the pipeline's own channel while the
    /// pipeline is Working — the window the spec says events are ignored in. Nothing else
    /// can reproduce it: `handle` runs the whole dictation synchronously.
    /// The sender is dropped as it is used: holding it would keep the channel connected and
    /// `pump` would block on `recv` for ever instead of returning.
    struct PressesWhileWorking {
        tx: Mutex<Option<std::sync::mpsc::Sender<HotkeyEvent>>>,
        event: HotkeyEvent,
    }
    impl Transcriber for PressesWhileWorking {
        fn transcribe(
            &self,
            _wav: &[u8],
            _language: &SttLanguage,
            _prompt: Option<&str>,
        ) -> Result<Transcript, String> {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                tx.send(self.event).expect("the pump still holds rx");
            }
            Ok(Transcript {
                text: "hi there".into(),
                no_speech_prob: None,
            })
        }
    }

    /// A pipeline wired for `pump`: no minimum hold, so two events sent back to back are a
    /// dictation rather than a discarded tap.
    fn pump_rig(
        mode: HotkeyMode,
        cap: FakeCapture,
        stt: Box<dyn Transcriber>,
        ind: FakeIndicator,
    ) -> Pipeline {
        let policy = LanguagePolicy::from_config(&LanguageConfig::default()).unwrap();
        Pipeline::new(
            PipelineConfig {
                initial_mode: mode,
                min_hold: Duration::ZERO,
                polish_min_words: 0,
                prompt: None,
                trailing_space: false,
                polish_model: String::new(),
                no_speech_threshold: 0.6,
            },
            Box::new(cap),
            Box::new(FakeLayout(None)),
            policy,
            stt,
            None,
            vec![Box::new(FakeInject::new("type", false))],
            Box::new(ind),
            None,
        )
    }

    /// B1: an event that arrives during Transcribing/Polishing/Inserting must be ignored,
    /// not deferred. Obeyed afterwards it opens the microphone against a state machine that
    /// is back at Idle — and in toggle mode nothing pairs with it, so the recording never
    /// ends.
    #[test]
    fn an_event_arriving_while_working_never_starts_a_recording() {
        for (mode, start, stop) in [
            (HotkeyMode::Toggle, HotkeyEvent::Toggle, HotkeyEvent::Toggle),
            (
                HotkeyMode::Hold,
                HotkeyEvent::Pressed,
                HotkeyEvent::Released,
            ),
        ] {
            let (tx, rx) = std::sync::mpsc::channel();
            let cap = FakeCapture::new(16_000);
            let ind = FakeIndicator::default();
            let stt = PressesWhileWorking {
                tx: Mutex::new(Some(tx.clone())),
                event: start,
            };
            let mut p = pump_rig(mode, cap.clone(), Box::new(stt), ind.clone());

            tx.send(start).unwrap();
            tx.send(stop).unwrap();
            drop(tx); // the transcriber's clone is the last one, and it drops it as it sends
            pump(&mut p, &rx);

            assert_eq!(
                (cap.starts(), cap.stops()),
                (1, 1),
                "{mode:?}: the microphone reopened for a {start:?} delivered while Working"
            );
            assert_eq!(p.shared().lock().unwrap().state, "idle", "{mode:?}");
            assert_eq!(ind.0.lock().unwrap().last(), Some(&S::Done), "{mode:?}");
        }
    }

    /// The drain must not eat a press that queued behind something that never reached
    /// Working — a sub-`min_hold` tap closes the microphone in microseconds, and the press
    /// after it is a dictation the user is asking for.
    #[test]
    fn a_press_behind_a_discarded_tap_still_dictates() {
        let (tx, rx) = std::sync::mpsc::channel();
        let cap = FakeCapture::new(16_000);
        let ind = FakeIndicator::default();
        let mut p = pump_rig(
            HotkeyMode::Toggle,
            cap.clone(),
            Box::new(FakeTranscriber::ok("hi there")),
            ind.clone(),
        );
        p.cfg.min_hold = Duration::from_secs(10); // every hold here is a tap

        tx.send(HotkeyEvent::Toggle).unwrap(); // start
        tx.send(HotkeyEvent::Toggle).unwrap(); // stop: discarded as a tap
        tx.send(HotkeyEvent::Toggle).unwrap(); // the next dictation, already queued
        drop(tx);
        pump(&mut p, &rx);

        assert_eq!(cap.starts(), 2, "the press after a tap was dropped");
        assert_eq!(p.shared().lock().unwrap().state, "recording");
    }

    #[test]
    fn summary_drops_the_response_body_from_a_stage_error() {
        assert_eq!(
            summary("stt HTTP 500: {\"error\":\"SECRET TRANSCRIPT\"}"),
            "stt HTTP 500"
        );
        assert_eq!(
            summary("polish response has no content field: SECRET TRANSCRIPT"),
            "polish response has no content field"
        );
        // Both stages name themselves, so a dead endpoint never reduces to the bare word
        // `transport` on the tray, the tray tooltip and `byovox status`.
        assert_eq!(
            summary("stt transport: io: Connection refused"),
            "stt transport"
        );
        assert_eq!(
            summary("polish transport: connection refused"),
            "polish transport"
        );
        assert_eq!(summary("no colon at all"), "no colon at all");
    }
}
