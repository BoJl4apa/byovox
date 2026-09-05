"""Periodic purge of original audio past `webui.audio_retention_days`. Never touches
transcripts, chunks or summaries — only files matching a recording's `original.*` glob."""

from __future__ import annotations

import threading
import time
from datetime import datetime, timedelta, timezone

from .storage import Storage

CHECK_INTERVAL_S = 3600


def purge_old_audio(storage: Storage, retention_days: int) -> int:
    if retention_days <= 0:
        return 0
    cutoff = datetime.now(timezone.utc) - timedelta(days=retention_days)
    removed = 0
    for rec in storage.list():
        original = rec.existing_original()
        if original is None:
            continue
        mtime = datetime.fromtimestamp(original.stat().st_mtime, tz=timezone.utc)
        if mtime < cutoff:
            original.unlink(missing_ok=True)
            removed += 1
    return removed


def start_background(storage: Storage, retention_days: int) -> threading.Thread:
    def loop():
        while True:
            purge_old_audio(storage, retention_days)
            time.sleep(CHECK_INTERVAL_S)

    t = threading.Thread(target=loop, daemon=True)
    t.start()
    return t
