//! Opt-in per-dictation dump: WAV + one JSONL row. The only place transcript text is
//! written to disk, and only when `capture_log.enabled = true`.
//!
//! Consumes `pipeline::DictationRecord` as a `Recorder`; produces `<dir>/<ts>-<n>.wav`
//! and appends to `<dir>/dictations.jsonl`. Nothing here is fatal to a dictation: a write
//! that fails is logged with its path — never with the text — and the row is skipped, so
//! the JSONL never names a WAV that was not written.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::pipeline::{DictationRecord, Recorder};

pub struct CaptureLog {
    dir: PathBuf,
    /// Distinguishes two dictations that land in the same millisecond.
    n: u64,
}

impl CaptureLog {
    pub fn new(dir: PathBuf) -> Result<CaptureLog> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(CaptureLog { dir, n: 0 })
    }
}

impl Recorder for CaptureLog {
    fn record(&mut self, r: &DictationRecord<'_>) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
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
            "language": r.language.label(),
            "raw": r.raw,
            "polished": r.polished,
            "rung": r.rung,
            "stt_ms": r.stt_ms,
            "polish_ms": r.polish_ms,
            "inject_ms": r.inject_ms,
        });
        let path = self.dir.join("dictations.jsonl");
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| writeln!(f, "{row}"));
        if let Err(e) = appended {
            tracing::error!(error = %e, path = %path.display(), "capture log: appending row failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Audio;
    use crate::lang::{Lang, SttLanguage};
    use crate::pipeline::{DictationRecord, Recorder};
    use std::path::PathBuf;

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
        let mut log = CaptureLog::new(dir.clone()).unwrap();
        let audio = Audio {
            samples: vec![0; 1600],
        };
        let lang = SttLanguage::Explicit(Lang::parse("he").unwrap());
        log.record(&DictationRecord {
            audio: &audio,
            language: &lang,
            raw: "raw\nline",
            polished: Some("Polished."),
            rung: Some("type"),
            stt_ms: 600,
            polish_ms: 400,
            inject_ms: 10,
        });
        let jsonl = std::fs::read_to_string(dir.join("dictations.jsonl")).unwrap();
        let row: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
        assert_eq!(row["raw"], "raw\nline");
        assert_eq!(row["polished"], "Polished.");
        assert_eq!(row["language"], "he");
        assert_eq!(row["rung"], "type");
        assert_eq!(row["stt_ms"], 600);
        let wav = dir.join(row["wav"].as_str().unwrap());
        assert!(wav.exists(), "{}", wav.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_millisecond_dictations_keep_their_own_wav() {
        let dir = temp_dir("same_millisecond_dictations_keep_their_own_wav");
        let mut log = CaptureLog::new(dir.clone()).unwrap();
        let audio = Audio {
            samples: vec![0; 16],
        };
        let lang = SttLanguage::Auto { candidates: vec![] };
        for raw in ["one", "two"] {
            log.record(&DictationRecord {
                audio: &audio,
                language: &lang,
                raw,
                polished: None,
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
}
