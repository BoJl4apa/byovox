"""Append-only JSONL logging for every Whisper/LLM call this app makes.

Mirrors the line-delimited-JSON convention `src/capture_log.rs` uses for dictations: one
row per call, timestamp first, never raise past a logging failure into the pipeline.
"""

from __future__ import annotations

import json
import threading
import time
from pathlib import Path
from typing import Any

_lock = threading.Lock()


def log(path: Path, **fields: Any) -> None:
    row = {"ts": int(time.time() * 1000), **fields}
    line = json.dumps(row, ensure_ascii=False)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with _lock, path.open("a", encoding="utf-8") as f:
            f.write(line + "\n")
    except OSError:
        # A log write must never take down a recording's processing.
        pass
