from pathlib import Path

import pytest

from pywebui.storage import Storage


def test_create_makes_a_folder_with_metadata(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("My Recording.m4a")
    assert rec.root.exists()
    meta = rec.read_metadata()
    assert meta.original_filename == "My Recording.m4a"
    assert meta.status == "uploaded"
    assert meta.chunks == []


def test_set_status_persists(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.mp3")
    rec.set_status("transcribing")
    assert rec.read_metadata().status == "transcribing"
    rec.set_status("error", error="boom")
    meta = rec.read_metadata()
    assert meta.status == "error"
    assert meta.error == "boom"


def test_progress_fields_persist(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("recording.m4a")
    rec.update_progress(
        stage_detail="transcribing 2 / 5 chunks",
        total_chunks=5,
        completed_chunks=2,
        total_speech_s=120.0,
        processed_speech_s=45.0,
        eta_s=30.0,
    )
    meta = rec.read_metadata()
    assert meta.stage_detail == "transcribing 2 / 5 chunks"
    assert meta.completed_chunks == 2
    assert meta.total_speech_s == 120.0
    assert meta.eta_s == 30.0


def test_get_rejects_path_traversal(tmp_path: Path):
    storage = Storage(tmp_path)
    storage.create("a.mp3")
    assert storage.get("../secrets") is None
    assert storage.get("..") is None
    assert storage.get("nonexistent-id") is None


def test_list_orders_newest_first(tmp_path: Path):
    storage = Storage(tmp_path)
    storage.create("a.mp3")
    storage.create("b.mp3")
    names = [r.root.name for r in storage.list()]
    assert names == sorted(names, reverse=True)


def test_original_path_and_existing_original(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    assert rec.existing_original() is None
    dest = rec.original_path(".m4a")
    dest.write_bytes(b"fake audio")
    assert rec.existing_original() == dest


def test_reset_for_retry_clears_previous_output_but_keeps_status_uploaded(tmp_path: Path):
    from pywebui.storage import Chunk

    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.chunk_txt(1, "old-topic").write_text("stale", encoding="utf-8")
    rec.final_srt.write_text("stale srt", encoding="utf-8")
    rec.summary_txt.write_text("stale summary", encoding="utf-8")
    meta = rec.read_metadata()
    meta.status = "error"
    meta.error = "stt request failed: timed out"
    meta.chunks = [Chunk(index=1, title="old", start=1, end=2, slug="old-topic")]
    rec.write_metadata(meta)

    rec.reset_for_retry()

    meta = rec.read_metadata()
    assert meta.status == "uploaded"
    assert meta.error is None
    assert meta.chunks == []
    assert not rec.final_srt.exists()
    assert not rec.summary_txt.exists()
    assert list(rec.chunks_dir.glob("*")) == []


def test_reset_for_retry_keeps_transcription_checkpoint(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.speech_ranges_json.write_text("[]", encoding="utf-8")
    rec.audio_chunks_dir.mkdir()
    (rec.audio_chunks_dir / "manifest.json").write_text("[]", encoding="utf-8")
    rec.raw_srt.write_text("1\n00:00:00,000 --> 00:00:01,000\nhello\n", encoding="utf-8")
    rec.update_progress(total_chunks=4, completed_chunks=2, total_speech_s=40.0)

    rec.reset_for_retry()

    meta = rec.read_metadata()
    assert meta.completed_chunks == 2
    assert meta.total_chunks == 4
    assert rec.speech_ranges_json.exists()
    assert rec.raw_srt.exists()
    assert rec.audio_chunks_dir.exists()


def test_cleanup_audio_chunks_keeps_manifest(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.audio_chunks_dir.mkdir()
    (rec.audio_chunks_dir / "chunk-0000.wav").write_bytes(b"wav")
    (rec.audio_chunks_dir / "manifest.json").write_text("[]", encoding="utf-8")

    rec.cleanup_audio_chunks()

    assert not list(rec.audio_chunks_dir.glob("*.wav"))
    assert (rec.audio_chunks_dir / "manifest.json").exists()


def test_reset_for_recalculation_keeps_raw_transcript(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.raw_srt.write_text("1\n00:00:00,000 --> 00:00:01,000\nhello\n", encoding="utf-8")
    rec.final_srt.write_text("stale", encoding="utf-8")
    rec.summary_txt.write_text("stale", encoding="utf-8")
    rec.set_status("done")

    rec.reset_for_recalculation()

    assert rec.read_metadata().status == "uploaded"
    assert rec.raw_srt.exists()
    assert not rec.final_srt.exists()
    assert not rec.summary_txt.exists()


def test_reconcile_interrupted_marks_stuck_jobs_as_error(tmp_path: Path):
    storage = Storage(tmp_path)
    stuck = storage.create("a.m4a")
    stuck.set_status("transcribing")
    finished = storage.create("b.m4a")
    finished.set_status("done")

    marked = storage.reconcile_interrupted()

    assert marked == 1
    assert stuck.read_metadata().status == "error"
    assert "interrupted" in stuck.read_metadata().error
    assert finished.read_metadata().status == "done"
