import os
import time
from pathlib import Path

from pywebui.housekeeping import purge_old_audio
from pywebui.storage import Storage


def _age_file(path: Path, days: float) -> None:
    past = time.time() - days * 86400
    os.utime(path, (past, past))


def test_purge_removes_only_old_audio(tmp_path: Path):
    storage = Storage(tmp_path)
    old = storage.create("old.m4a")
    old.original_path(".m4a").write_bytes(b"x")
    _age_file(old.original_path(".m4a"), days=10)

    new = storage.create("new.m4a")
    new.original_path(".m4a").write_bytes(b"y")

    removed = purge_old_audio(storage, retention_days=7)

    assert removed == 1
    assert old.existing_original() is None
    assert new.existing_original() is not None


def test_zero_retention_disables_purge(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.original_path(".m4a").write_bytes(b"x")
    _age_file(rec.original_path(".m4a"), days=365)

    removed = purge_old_audio(storage, retention_days=0)

    assert removed == 0
    assert rec.existing_original() is not None


def test_purge_never_touches_transcripts(tmp_path: Path):
    storage = Storage(tmp_path)
    rec = storage.create("a.m4a")
    rec.original_path(".m4a").write_bytes(b"x")
    rec.final_srt.write_text("1\n00:00:00,000 --> 00:00:01,000\nhi\n\n", encoding="utf-8")
    _age_file(rec.original_path(".m4a"), days=10)

    purge_old_audio(storage, retention_days=7)

    assert rec.final_srt.exists()
