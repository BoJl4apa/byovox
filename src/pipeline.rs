//! The dictation state machine. Knows no OS: everything platform-specific arrives as a
//! boxed trait. One `handle` call per hotkey event; the return value is the outcome of a
//! finished dictation, if one finished.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::Audio;
use crate::capture::Capture;
use crate::hotkey::{HotkeyEvent, HotkeyMode};
use crate::indicator::{Indicator, IndicatorState};
use crate::inject::Inject;
use crate::lang::{LanguagePolicy, SttLanguage};
use crate::layout::Layout;
use crate::polish::{Polisher, word_count};
use crate::stt::Transcriber;

pub struct PipelineConfig {
    pub mode: HotkeyMode,
    pub min_hold: Duration,
    pub polish_min_words: usize,
    pub prompt: Option<String>,
    pub trailing_space: bool,
}

/// What `byovox status` / `byovox last` read; written by the pipeline thread.
#[derive(Debug)]
pub struct Shared {
    pub state: &'static str,
    /// The tray's Enable/Disable toggle. A disabled pipeline starts nothing; an event
    /// arriving during a recording closes the microphone and discards the audio.
    pub enabled: bool,
    pub last_transcript: Option<String>,
    pub last_error: Option<String>,
}

impl Default for Shared {
    /// A fresh pipeline is idle and listening; `bool::default()` would make it deaf.
    fn default() -> Shared {
        Shared {
            state: "idle",
            enabled: true,
            last_transcript: None,
            last_error: None,
        }
    }
}

/// One finished dictation, for the opt-in capture log.
pub struct DictationRecord<'a> {
    pub audio: &'a Audio,
    pub language: &'a SttLanguage,
    pub raw: &'a str,
    pub polished: Option<&'a str>,
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

/// The head of a stage error, for WARN and above and for `Shared::last_error`: everything
/// before the first `:`. Stage errors carry a response-body prefix after the colon, which
/// can be transcript text — the full string goes to `debug!`, this summary goes everywhere
/// a human can see it (the log line, the tray menu, `byovox status`).
pub(crate) fn summary(e: &str) -> &str {
    e.split_once(':').map_or(e, |(head, _)| head).trim()
}

enum State {
    Idle,
    Recording {
        since: Instant,
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
        let shared = Arc::new(Mutex::new(Shared::default()));
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
        // The tray flips `enabled` from another thread, possibly mid-dictation. A disabled
        // pipeline is deaf, but it never leaves the microphone open: the first event to
        // arrive during a recording closes it and drops the audio.
        if !self.shared.lock().unwrap().enabled {
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
        match (ev, self.cfg.mode, recording) {
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
        let language = self.policy.resolve(self.layout.current());
        if let Err(e) = self.capture.start() {
            tracing::debug!(error = %e, "microphone open error");
            tracing::error!(error = summary(&e), "microphone open failed");
            self.fail(e);
            return;
        }
        self.set_state(
            State::Recording {
                since: now,
                language,
            },
            IndicatorState::Recording,
        );
    }

    fn finish(&mut self, now: Instant) -> Outcome {
        let State::Recording { since, language } = std::mem::replace(&mut self.state, State::Idle)
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
        let raw = match self
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
        tracing::debug!(raw = %raw, "transcript");
        if raw.trim().is_empty() {
            self.set_state(State::Idle, IndicatorState::Idle);
            tracing::info!(lang = %language.label(), stt_ms, "empty transcript");
            return Outcome::Empty;
        }

        let mut errored = false;
        let t = Instant::now();
        let polished = match &self.polish {
            Some(p) if word_count(&raw) >= self.cfg.polish_min_words => match p.polish(&raw) {
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
            _ => None,
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
                language: &language,
                raw: &raw,
                polished: polished.as_deref(),
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
                let final_state = if errored {
                    IndicatorState::Error
                } else {
                    IndicatorState::Idle
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
            mode: HotkeyMode::Hold,
            min_hold: Duration::from_millis(250),
            polish_min_words: 0,
            prompt: Some("Glossary: Acme".into()),
            trailing_space: false,
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
            [S::Recording, S::Working, S::Idle]
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
    }

    #[test]
    fn toggle_mode_starts_and_stops_on_toggle() {
        let mut r = rig(FakeTranscriber::ok("hi"), None, false, false);
        r.p.cfg.mode = HotkeyMode::Toggle;
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
    fn disabled_pipeline_ignores_every_event() {
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
            FakeTranscriber::ok("um hello"),
            Some(FakePolisher::ok("Hello.")),
            false,
            false,
        );
        dictate(&mut r, Duration::from_secs(1));
        let recs = r.rec.0.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].samples, 16_000);
        assert_eq!(recs[0].language, "he");
        assert_eq!(recs[0].raw, "um hello");
        assert_eq!(recs[0].polished.as_deref(), Some("Hello."));
        assert_eq!(recs[0].rung, Some("type"));
        // Wall-clock timings: only a sanity bound is non-flaky, but it pins them as wired.
        for ms in [recs[0].stt_ms, recs[0].polish_ms, recs[0].inject_ms] {
            assert!(
                ms < 60_000,
                "{ms} ms is not a plausible fake-backend timing"
            );
        }
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
        assert_eq!(
            summary("polish transport: connection refused"),
            "polish transport"
        );
        assert_eq!(summary("no colon at all"), "no colon at all");
    }
}
