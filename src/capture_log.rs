//! Opt-in per-dictation dump: WAV + one JSONL row. The only place transcript text is
//! written to disk, and only when `capture_log.enabled = true`.
//!
//! Consumes `pipeline::DictationRecord` as a `Recorder`; produces `<dir>/<ts>-<n>.wav`
//! and appends to `<dir>/dictations.jsonl`. Nothing here is fatal to a dictation: a write
//! that fails is logged with its path — never with the text — and the row is skipped, so
//! the JSONL never names a WAV that was not written.
//!
//! Retention is `capture_log.keep_days`: this corpus is voice recordings and verbatim
//! transcripts, so it prunes itself rather than growing without bound.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::pipeline::{DictationRecord, Recorder};

/// Milliseconds in a day — the unit `keep_days` is counted in, against the `ts` that the file
/// names and the rows already carry.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The JSONL file, named once: `record` appends to it and `prune_rows` rewrites it.
const ROWS: &str = "dictations.jsonl";

pub struct CaptureLog {
    dir: PathBuf,
    /// Distinguishes two dictations that land in the same millisecond.
    n: u64,
    /// `capture_log.keep_days`; `0` disables pruning entirely.
    keep_days: u32,
}

impl CaptureLog {
    pub fn new(dir: PathBuf, keep_days: u32) -> Result<CaptureLog> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let log = CaptureLog {
            dir,
            n: 0,
            keep_days,
        };
        // At startup as well as after each write: a daemon restarted with a shorter retention
        // than it ran with must act on the change without waiting for the next dictation, and
        // a machine left off for a month must not come back to a stale corpus.
        log.prune();
        Ok(log)
    }

    /// The `ts` below which a capture has expired, or `None` when nothing ever expires.
    fn cutoff(&self) -> Option<u64> {
        if self.keep_days == 0 {
            return None;
        }
        Some(now_ms().saturating_sub(u64::from(self.keep_days) * DAY_MS))
    }

    /// Delete expired WAVs and drop expired JSONL rows.
    ///
    /// Cheap in the common case, which matters because this runs after every dictation. The
    /// WAV pass reads directory entries only — the timestamp is in the name, so nothing is
    /// stat-ed — and the row pass reads the first line and stops there while it is still
    /// fresh, which it is on every run but the first of each day, because rows are appended
    /// in time order.
    ///
    /// Failures are logged and swallowed: retention is housekeeping, and a directory that
    /// cannot be pruned is no reason to lose a dictation.
    fn prune(&self) {
        let Some(cutoff) = self.cutoff() else { return };
        self.prune_wavs(cutoff);
        self.prune_rows(cutoff);
    }

    /// Only files this log itself named — `<ts>-<n>.wav`, both halves parsing as integers.
    /// Anything else in the directory belongs to someone else and is never touched.
    fn prune_wavs(&self, cutoff: u64) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, path = %self.dir.display(), "capture log: prune could not read the directory");
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(ts) = name.to_str().and_then(wav_timestamp) else {
                continue;
            };
            if ts < cutoff {
                let path = entry.path();
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not delete a wav");
                }
            }
        }
    }

    /// Rewrite the JSONL without its expired rows, and only when there are any.
    ///
    /// A row whose `ts` cannot be read is kept: this function deletes data, so anything it
    /// does not understand survives rather than being discarded on a guess.
    fn prune_rows(&self, cutoff: u64) {
        let path = self.dir.join(ROWS);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // No file yet is the ordinary state before the first dictation.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not read the rows");
                return;
            }
        };
        // Rows are appended in time order, so a fresh first row means no row is expired.
        // This is the entire cost of a prune on all but the first write of each day.
        if text
            .lines()
            .next()
            .and_then(row_timestamp)
            .is_some_and(|ts| ts >= cutoff)
        {
            return;
        }
        let total = text.lines().count();
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| row_timestamp(l).is_none_or(|ts| ts >= cutoff))
            .collect();
        if kept.len() == total {
            return;
        }
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        if let Err(e) = std::fs::write(&path, body) {
            tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not rewrite the rows");
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| capped(d.as_millis()))
        .unwrap_or(0)
}

/// The timestamp in a WAV name this log wrote, or `None` for any other file. Both halves must
/// parse, so `notes-2024.wav`, `dictations.jsonl` and anything a user dropped in the
/// directory are left alone.
fn wav_timestamp(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".wav")?;
    let (ts, n) = stem.split_once('-')?;
    n.parse::<u64>().ok()?;
    ts.parse::<u64>().ok()
}

/// The `ts` of one JSONL row, or `None` when the line is not a row this log would recognise.
fn row_timestamp(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("ts")?
        .as_u64()
}

/// `serde_json::json!` panics on a `u128` past `u64::MAX`; saturating keeps `record` total.
fn capped(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

impl Recorder for CaptureLog {
    fn record(&mut self, r: &DictationRecord<'_>) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| capped(d.as_millis()))
            .unwrap_or(0);
        let wav_name = format!("{ts}-{}.wav", self.n);
        self.n += 1;
        let wav_path = self.dir.join(&wav_name);
        if let Err(e) = std::fs::write(&wav_path, r.audio.to_wav()) {
            tracing::error!(error = %e, path = %wav_path.display(), "capture log: writing wav failed");
            return;
        }
        let row = serde_json::json!({
            "ts": ts,
            "wav": wav_name,
            // The layout that routed the dictation and the language it resolved to: only
            // the pair answers "which keyboard layout produced this", and only the model
            // makes rows comparable across a `polish.model` change.
            "layout": r.layout.map(|l| l.to_string()),
            "language": r.language.label(),
            "raw": r.raw,
            // What whisper scored the clip, so a row the no-speech gate dropped can be told
            // from one it let through — and so the threshold can be re-tuned from the corpus.
            "no_speech_prob": r.no_speech_prob,
            "polished": r.polished,
            "polish_model": r.polish_model,
            "rung": r.rung,
            "stt_ms": capped(r.stt_ms),
            "polish_ms": capped(r.polish_ms),
            "inject_ms": capped(r.inject_ms),
        });
        let path = self.dir.join(ROWS);
        // One `write_all`, newline included: `writeln!` on an unbuffered file streams the row
        // in fragments, so a failure mid-row leaves a newline-less stub that the next row is
        // appended to — one unparseable line where there should have been two.
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(format!("{row}\n").as_bytes()));
        if let Err(e) = appended {
            tracing::error!(error = %e, path = %path.display(), "capture log: appending row failed");
        }
        // After the write, not before: the row just added is the newest, so pruning here can
        // only ever remove older ones, and the fresh-first-row check makes it a single line
        // read on all but the first dictation of each day.
        self.prune();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Audio;
    use crate::lang::{Lang, SttLanguage};
    use crate::pipeline::{DictationRecord, Recorder};
    use std::path::Path;

    /// A directory of this test's own: the whole suite shares one process id.
    fn temp_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("byovox-capture-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn writes_wav_and_jsonl_row() {
        let dir = temp_dir("writes_wav_and_jsonl_row");
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        let audio = Audio {
            samples: vec![0; 1600],
        };
        let lang = SttLanguage::Explicit(Lang::parse("he").unwrap());
        log.record(&DictationRecord {
            audio: &audio,
            layout: Lang::parse("ru"),
            language: &lang,
            raw: "raw\nline",
            no_speech_prob: Some(0.04),
            polished: Some("Polished."),
            polish_model: Some("cleanup-1"),
            rung: Some("type"),
            stt_ms: 600,
            polish_ms: 400,
            inject_ms: 10,
        });
        let jsonl = std::fs::read_to_string(dir.join("dictations.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(row["raw"], "raw\nline");
        assert_eq!(row["no_speech_prob"], 0.04_f32);
        assert_eq!(row["polished"], "Polished.");
        // The layout is the question, the language the policy's answer: a row carrying only
        // the answer cannot say which layout routed the dictation.
        assert_eq!(row["layout"], "ru");
        assert_eq!(row["language"], "he");
        assert_eq!(row["polish_model"], "cleanup-1");
        assert_eq!(row["rung"], "type");
        assert_eq!(row["stt_ms"], 600);
        assert_eq!(row["polish_ms"], 400);
        assert_eq!(row["inject_ms"], 10);
        assert!(row["ts"].is_number(), "ts: {}", row["ts"]);
        let wav = dir.join(row["wav"].as_str().unwrap());
        assert!(wav.exists(), "{}", wav.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_millisecond_dictations_keep_their_own_wav() {
        let dir = temp_dir("same_millisecond_dictations_keep_their_own_wav");
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        let audio = Audio {
            samples: vec![0; 16],
        };
        let lang = SttLanguage::Auto { candidates: vec![] };
        for raw in ["one", "two"] {
            log.record(&DictationRecord {
                audio: &audio,
                layout: None,
                language: &lang,
                raw,
                no_speech_prob: None,
                polished: None,
                polish_model: None,
                rung: None,
                stt_ms: 1,
                polish_ms: 0,
                inject_ms: 0,
            });
        }
        let jsonl = std::fs::read_to_string(dir.join("dictations.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        let wavs: Vec<&str> = rows.iter().map(|r| r["wav"].as_str().unwrap()).collect();
        assert_ne!(
            wavs[0], wavs[1],
            "one row would reference the other's audio"
        );
        for wav in wavs {
            assert!(dir.join(wav).exists(), "{wav}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fabricate a capture at a chosen age. The timestamp lives in the WAV's name and in the
    /// row's `ts`, so a test can age a capture exactly without touching file mtimes — which
    /// is also why pruning never needs to stat anything.
    fn plant(dir: &Path, days_ago: u64, n: u64) -> String {
        let ts = now_ms() - days_ago * DAY_MS;
        let wav = format!("{ts}-{n}.wav");
        std::fs::write(dir.join(&wav), b"RIFFfake").unwrap();
        let row = serde_json::json!({"ts": ts, "wav": wav, "raw": "planted"});
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(ROWS))
            .unwrap();
        f.write_all(format!("{row}\n").as_bytes()).unwrap();
        wav
    }

    /// The corpus is voice and verbatim text, so it has to stop growing on its own. Startup
    /// prunes, because a retention shortened between runs must take effect without waiting
    /// for a dictation.
    #[test]
    fn captures_past_keep_days_are_pruned_at_startup() {
        let dir = temp_dir("prune_at_startup");
        std::fs::create_dir_all(&dir).unwrap();
        let old = plant(&dir, 40, 0);
        let recent = plant(&dir, 3, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(!dir.join(&old).exists(), "a 40-day-old wav survived");
        assert!(dir.join(&recent).exists(), "a 3-day-old wav was deleted");
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains(&recent), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `0` is the setting for someone who is deliberately building a corpus — the
    /// maintainer's own deployment — and it must keep everything, however old.
    #[test]
    fn keep_days_zero_prunes_nothing() {
        let dir = temp_dir("prune_disabled");
        std::fs::create_dir_all(&dir).unwrap();
        let ancient = plant(&dir, 4_000, 0);

        let _log = CaptureLog::new(dir.clone(), 0).unwrap();

        assert!(dir.join(&ancient).exists());
        assert_eq!(
            std::fs::read_to_string(dir.join(ROWS))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pruning deletes files, so it only ever touches names this log itself wrote. Anything
    /// else in the directory — a note, an export, the JSONL — is left alone however old.
    #[test]
    fn pruning_never_touches_a_file_the_log_did_not_name() {
        let dir = temp_dir("prune_only_own_files");
        std::fs::create_dir_all(&dir).unwrap();
        plant(&dir, 90, 0);
        for stranger in ["notes.wav", "holiday-2019.wav", "2019.wav", "export.txt"] {
            std::fs::write(dir.join(stranger), b"not mine").unwrap();
        }

        let _log = CaptureLog::new(dir.clone(), 1).unwrap();

        for stranger in ["notes.wav", "holiday-2019.wav", "2019.wav", "export.txt"] {
            assert!(dir.join(stranger).exists(), "{stranger} was deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name test on its own, including the shapes a careless split would eat.
    #[test]
    fn only_a_timestamp_dash_counter_wav_is_ours() {
        assert_eq!(
            wav_timestamp("1700000000000-0.wav"),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            wav_timestamp("1700000000000-12.wav"),
            Some(1_700_000_000_000)
        );
        for other in [
            "notes.wav",
            "holiday-2019.wav",
            "2019.wav",
            "1700000000000-.wav",
            "-0.wav",
            "1700000000000-0.txt",
            ROWS,
        ] {
            assert_eq!(wav_timestamp(other), None, "{other}");
        }
    }

    /// A row the pruner cannot read is kept: this code deletes data, so anything it does not
    /// understand has to survive rather than be discarded on a guess.
    #[test]
    fn an_unreadable_row_is_kept_while_expired_ones_go() {
        let dir = temp_dir("prune_keeps_unreadable_rows");
        std::fs::create_dir_all(&dir).unwrap();
        // A corrupt first line must also not stop the pass reaching the rows behind it.
        std::fs::write(dir.join(ROWS), "{ this is not json\n").unwrap();
        plant(&dir, 90, 0);
        plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert!(rows.contains("not json"), "the unreadable row went: {rows}");
        assert_eq!(rows.lines().count(), 2, "the 90-day row survived: {rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writing prunes too, so a daemon left running for months does not accumulate — the
    /// startup pass alone would never fire on a box that never reboots.
    #[test]
    fn a_write_prunes_what_startup_left_behind() {
        let dir = temp_dir("prune_after_write");
        std::fs::create_dir_all(&dir).unwrap();
        let mut log = CaptureLog::new(dir.clone(), 30).unwrap();
        // Planted after construction, so only the post-write prune can remove it.
        let old = plant(&dir, 60, 7);
        assert!(dir.join(&old).exists());

        let audio = Audio {
            samples: vec![0; 16],
        };
        let lang = SttLanguage::Auto { candidates: vec![] };
        log.record(&DictationRecord {
            audio: &audio,
            layout: None,
            language: &lang,
            raw: "fresh",
            no_speech_prob: None,
            polished: None,
            polish_model: None,
            rung: None,
            stt_ms: 1,
            polish_ms: 0,
            inject_ms: 0,
        });

        assert!(!dir.join(&old).exists(), "the write did not prune");
        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains("fresh"), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
