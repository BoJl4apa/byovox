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
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::pipeline::{DictationRecord, Recorder};

/// Milliseconds in a day — the unit `keep_days` is counted in, against the `ts` that the file
/// names and the rows already carry.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// The JSONL file, named once: `record` appends to it and `prune` rewrites it. It is also the
/// index of what this log owns — the only thing that authorises deleting a WAV.
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

    /// Drop expired JSONL rows and delete the WAVs those rows name.
    ///
    /// **The JSONL is the index, and the only source of deletion candidates.** The directory
    /// is never scanned: a name is not evidence of ownership, and a user's own `2024-1.wav`
    /// sitting in this directory would pass any shape test loose enough to match the real
    /// thing. A capture is deleted because a row this log wrote says it belongs to that row,
    /// and the name on the row is checked against the exact shape `record` writes as a second
    /// gate. A WAV with no row is therefore never touched — including one orphaned by a JSONL
    /// append that failed after its audio was written. That is the deliberate trade: an
    /// occasional orphan the user can delete, rather than a pruner that can reach a file it
    /// did not create.
    ///
    /// What it does, in order: read the whole JSONL; if the **first** row is still fresh,
    /// return — rows are appended in time order, so a fresh first row means no row is expired,
    /// and that is the path taken on every run but the first of each day, which is what keeps
    /// this cheap enough to run after every dictation. Otherwise partition the lines into
    /// expired and kept, delete the WAV each expired row names, and replace the file with the
    /// kept rows via a temp file and a rename, so the JSONL is never observed half-written.
    ///
    /// The whole file is read and rewritten on that slower path. It is bounded by the corpus,
    /// which is what `keep_days` exists to bound.
    ///
    /// Failures are logged and swallowed: retention is housekeeping, and a directory that
    /// cannot be pruned is no reason to lose a dictation.
    fn prune(&self) {
        let Some(cutoff) = self.cutoff() else { return };
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
        // A row whose `ts` cannot be read is kept: this deletes data, so anything it does not
        // understand survives rather than being discarded on a guess.
        let (expired, kept): (Vec<&str>, Vec<&str>) = text
            .lines()
            .partition(|l| row_timestamp(l).is_some_and(|ts| ts < cutoff));
        if expired.is_empty() {
            return;
        }
        for row in &expired {
            self.remove_capture(row);
        }
        let mut body = kept.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        // The pid keeps two byovox processes — the loser of a single-instance race, or a
        // second one pointed at the same capture directory by `--config` — from writing each
        // other's temp file half-way through.
        let tmp = self.dir.join(format!("{ROWS}.tmp-{}", std::process::id()));
        if let Err(e) = replace_atomically(&path, &tmp, &body) {
            tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not rewrite the rows");
            // The original is still whole; the temp file is the only casualty.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Delete the WAV one expired row names, if that row names one this log could have
    /// written. The name is checked rather than trusted: the row is read back off disk, and a
    /// `wav` field holding a path — `../../something` — must not be able to steer a delete out
    /// of this directory. `is_own_capture_name` admits no separators, so the join is safe.
    ///
    /// A file that is already gone is not an error: the row is dropped either way, which is
    /// what makes pruning idempotent after a partial run.
    fn remove_capture(&self, row: &str) {
        let Some(name) = serde_json::from_str::<serde_json::Value>(row)
            .ok()
            .and_then(|v| v.get("wav")?.as_str().map(str::to_owned))
            .filter(|n| is_own_capture_name(n))
        else {
            return;
        };
        let path = self.dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "capture log: prune could not delete a wav");
            }
        }
    }
}

/// Replace `path`'s contents with `body` without ever leaving it half-written.
///
/// `std::fs::write` is `File::create` — which **truncates** — followed by `write_all`. Between
/// those two the file is empty, so a crash, a full disk or a killed process at that moment
/// destroys every row the cutoff had decided to keep. That is a retention pass losing the data
/// it exists to protect, on the one file byovox cannot regenerate.
///
/// Instead the kept rows go to a sibling temp file, are flushed to the disk itself, and are
/// then renamed over the original. `rename` replaces atomically on both POSIX and Windows
/// (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`), so a reader sees either every old row or
/// every kept row and never a truncated file. The temp file must be a *sibling*: rename is
/// only atomic within one volume.
///
/// `sync_all` before the rename, not after: without it the directory entry can reach the disk
/// pointing at blocks that never did, which is exactly the torn file this avoids. The handle
/// is dropped before the rename because Windows refuses to rename an open file.
fn replace_atomically(path: &Path, tmp: &Path, body: &str) -> std::io::Result<()> {
    {
        let mut f = std::fs::File::create(tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| capped(d.as_millis()))
        .unwrap_or(0)
}

/// Whether `name` is the exact shape `record` writes: **thirteen** digits of unix
/// milliseconds, `-`, a decimal counter, `.wav`.
///
/// The digit count is the whole point. "two integers separated by a dash" also describes
/// `2024-1.wav` and `2019-12.wav` — plausible names in anyone's audio folder — and a pruner
/// that accepted those would read `2024` as a timestamp from 1970 and delete a file it never
/// created. Unix milliseconds have been 13 digits since 2001-09-09 and stay 13 until 2286.
///
/// This is a second gate, not the first: the caller only ever asks about a name that an
/// expired row already claimed. It also keeps a `wav` field holding a path out of the join —
/// no separator, dot or sign survives an all-ASCII-digits test.
fn is_own_capture_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".wav") else {
        return false;
    };
    let Some((ts, n)) = stem.split_once('-') else {
        return false;
    };
    ts.len() == 13
        && ts.bytes().all(|b| b.is_ascii_digit())
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
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

    /// Every file shape both reviewers pointed at. `2024-1.wav` and `2019-12.wav` are the
    /// ones that mattered: a user's own recordings, two integers and a dash, which the old
    /// pruner read as a 1970 timestamp and destroyed. `1-1.wav`, `0-0.wav` and
    /// `20240101-1.wav` are the same defect at other digit counts.
    ///
    /// The retention here is 1 day and every stranger is far "older" than that by any reading
    /// of its name, so a pruner that still scanned the directory would take all of them.
    const STRANGERS: [&str; 9] = [
        "2024-1.wav",
        "2019-12.wav",
        "1-1.wav",
        "0-0.wav",
        "20240101-1.wav",
        "notes.wav",
        "holiday-2019.wav",
        "2019.wav",
        "export.txt",
    ];

    #[test]
    fn pruning_never_touches_a_file_the_log_did_not_name() {
        let dir = temp_dir("prune_only_own_files");
        std::fs::create_dir_all(&dir).unwrap();
        plant(&dir, 90, 0);
        for stranger in STRANGERS {
            std::fs::write(dir.join(stranger), b"not mine").unwrap();
        }

        let _log = CaptureLog::new(dir.clone(), 1).unwrap();

        for stranger in STRANGERS {
            assert!(dir.join(stranger).exists(), "{stranger} was deleted");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A capture is deleted because a row named it, not because its name looked right. A WAV
    /// this log really did write, but that no row claims, is left alone — the case an
    /// orphaned audio file (JSONL append failed after the audio landed) falls into.
    #[test]
    fn a_wav_with_no_row_is_never_deleted() {
        let dir = temp_dir("prune_orphan_wav");
        std::fs::create_dir_all(&dir).unwrap();
        // A real, correctly-shaped, long-expired capture name — with no row referencing it.
        let orphan = "1700000000000-0.wav";
        std::fs::write(dir.join(orphan), b"RIFForphan").unwrap();
        // A separate expired capture that *does* have a row, so the pass actually runs.
        let claimed = plant(&dir, 90, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        assert!(
            dir.join(orphan).exists(),
            "an unclaimed wav was deleted; only rows may name a deletion"
        );
        assert!(!dir.join(&claimed).exists(), "the claimed wav survived");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired row whose audio is already gone must still drop the row, quietly: that is
    /// what makes a prune interrupted half-way idempotent on the next run.
    #[test]
    fn an_expired_row_whose_wav_is_missing_drops_the_row_without_error() {
        let dir = temp_dir("prune_missing_wav");
        std::fs::create_dir_all(&dir).unwrap();
        let gone = plant(&dir, 90, 0);
        std::fs::remove_file(dir.join(&gone)).unwrap();
        let fresh = plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let rows = std::fs::read_to_string(dir.join(ROWS)).unwrap();
        assert_eq!(rows.lines().count(), 1, "{rows}");
        assert!(rows.contains(&fresh), "{rows}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `wav` field is read back off disk, so it is input. A row claiming a path must not be
    /// able to steer a delete out of the capture directory.
    #[test]
    fn a_row_naming_a_path_cannot_reach_outside_the_directory() {
        let dir = temp_dir("prune_traversal");
        let outside = dir.join("outside.wav");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(&outside, b"not a capture").unwrap();

        let inner = dir.join("inner");
        let ts = now_ms() - 90 * DAY_MS;
        for claim in ["../outside.wav", "..\\outside.wav", "/outside.wav"] {
            let row = serde_json::json!({"ts": ts, "wav": claim, "raw": "x"});
            std::fs::write(inner.join(ROWS), format!("{row}\n")).unwrap();
            let _log = CaptureLog::new(inner.clone(), 30).unwrap();
            assert!(outside.exists(), "`{claim}` reached outside the directory");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The name test on its own, including the shapes a careless split would eat.
    #[test]
    fn only_a_timestamp_dash_counter_wav_is_ours() {
        assert!(is_own_capture_name("1700000000000-0.wav"));
        assert!(is_own_capture_name("1700000000000-12.wav"));
        // The name `record` writes right now, whatever the clock says.
        assert!(is_own_capture_name(&format!("{}-0.wav", now_ms())));
        for other in [
            // The reviewers' cases: two integers and a dash, but not thirteen digits.
            "2024-1.wav",
            "2019-12.wav",
            "1-1.wav",
            "0-0.wav",
            "20240101-1.wav",
            // Fourteen digits is not thirteen either.
            "17000000000000-0.wav",
            "notes.wav",
            "holiday-2019.wav",
            "2019.wav",
            "1700000000000-.wav",
            "-0.wav",
            "1700000000000-0.txt",
            // A signed or spaced number is not all-digits.
            "+700000000000-0.wav",
            "1700000000000- 0.wav",
            // Paths never survive the digit test, which is what keeps the join in bounds.
            "../1700000000000-0.wav",
            "sub/1700000000000-0.wav",
            ROWS,
        ] {
            assert!(!is_own_capture_name(other), "{other} was claimed as ours");
        }
    }

    /// The rewrite must be all-or-nothing. `std::fs::write` truncates before it writes, so a
    /// failure part-way through left an empty or half-written JSONL and destroyed exactly the
    /// rows the cutoff had decided to keep.
    ///
    /// The failure is injected through the temp path — a directory that does not exist, so
    /// `File::create` fails before anything touches the original — because the interesting
    /// property is "on any error the original is untouched", not which error it was.
    #[test]
    fn a_failed_rewrite_leaves_the_original_bytes_intact() {
        let dir = temp_dir("atomic_rewrite_failure");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ROWS);
        let original = "{\"ts\":1,\"wav\":\"a\"}\n{\"ts\":2,\"wav\":\"b\"}\n";
        std::fs::write(&path, original).unwrap();

        let doomed = dir.join("no-such-subdir").join("x.tmp");
        let err = replace_atomically(&path, &doomed, "replacement").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a failed rewrite must not have touched the original"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The success path, and the guarantee that makes it atomic: the temp file is consumed by
    /// the rename, so no debris is left in a directory the user browses.
    #[test]
    fn a_successful_rewrite_replaces_the_file_and_leaves_no_temp() {
        let dir = temp_dir("atomic_rewrite_success");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ROWS);
        std::fs::write(&path, "old\n").unwrap();
        let tmp = dir.join(format!("{ROWS}.tmp-{}", std::process::id()));

        replace_atomically(&path, &tmp, "new\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(!tmp.exists(), "the temp file survived the rename");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The temp file is a sibling of the JSONL, not a system temp file: rename is only atomic
    /// within a volume, and a capture directory can sit on a different drive.
    #[test]
    fn a_real_prune_leaves_no_temp_file_behind() {
        let dir = temp_dir("prune_leaves_no_temp");
        std::fs::create_dir_all(&dir).unwrap();
        plant(&dir, 90, 0);
        plant(&dir, 1, 1);

        let _log = CaptureLog::new(dir.clone(), 30).unwrap();

        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
