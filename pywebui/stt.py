"""STT client: one multipart POST per audio chunk to `{stt.base_url}/audio/transcriptions`,
requesting `verbose_json` for segment timestamps. Mirrors the wire format `src/stt.rs`
sends (see `Multipart`/`SttClient::transcribe`) but stdlib-only (`urllib`), matching
`bench/polish_bench.py`'s convention of no third-party HTTP library.
"""

from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Sequence

from . import srt
from .config import SttConfig
from .logging_json import log as log_jsonl


def _multipart(fields: list[tuple[str, str]], file_field: str, filename: str, data: bytes):
    boundary = f"byovox{uuid.uuid4().hex}"
    body = bytearray()
    for name, value in fields:
        body += f"--{boundary}\r\n".encode()
        body += f'Content-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
    body += f"--{boundary}\r\n".encode()
    body += (
        f'Content-Disposition: form-data; name="{file_field}"; filename="{filename}"\r\n'
        f"Content-Type: audio/wav\r\n\r\n"
    ).encode()
    body += data
    body += b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    return f"multipart/form-data; boundary={boundary}", bytes(body)


def transcribe_chunk(
    stt: SttConfig,
    wav_path: Path,
    start_offset: float,
    log_path: Path,
    recording_id: str,
    source_ranges: Sequence[tuple[float, float]] | None = None,
) -> list[srt.Segment]:
    content_type, body = _multipart(
        fields=[
            ("model", stt.model),
            ("response_format", "verbose_json"),
        ] + ([
            ("language_candidates", ",".join(stt.language_candidates)),
        ] if stt.language_candidates else []),
        file_field="file",
        filename=wav_path.name,
        data=wav_path.read_bytes(),
    )
    url = f"{stt.base_url.rstrip('/')}/audio/transcriptions"
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", content_type)
    token = stt.token()
    if token:
        req.add_header("Authorization", f"Bearer {token}")

    started = time.monotonic()
    status = None
    error = None
    text_preview = ""
    try:
        with urllib.request.urlopen(req, timeout=stt.timeout_s) as resp:
            status = resp.status
            raw = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        status = e.code
        error = e.read().decode("utf-8", errors="replace")[:500]
        raw = None
    except (urllib.error.URLError, TimeoutError) as e:
        error = str(e)
        raw = None
    latency_ms = round((time.monotonic() - started) * 1000)

    segments: list[srt.Segment] = []
    ranges = list(source_ranges or [])

    def source_time(local_time: float) -> float:
        if not ranges:
            return start_offset + local_time
        remaining = max(0.0, local_time)
        for source_start, source_end in ranges:
            length = source_end - source_start
            if remaining <= length:
                return source_start + remaining
            remaining -= length
        return ranges[-1][1]

    if raw is not None:
        for s in raw.get("segments") or []:
            segments.append(
                srt.Segment(
                    start=source_time(float(s.get("start", 0.0))),
                    end=source_time(float(s.get("end", 0.0))),
                    text=str(s.get("text", "")),
                )
            )
        if not segments and raw.get("text"):
            # A server that answers plain `json` (no segments) still gives usable text; one
            # segment spanning the whole chunk beats losing the audio's words entirely.
            segments.append(
                srt.Segment(start=source_time(0.0), end=source_time(0.0), text=str(raw["text"]))
            )
        text_preview = (raw.get("text") or "")[:200]

    log_jsonl(
        log_path,
        recording_id=recording_id,
        stage="stt",
        chunk=wav_path.name,
        url=url,
        model=stt.model,
        status=status,
        error=error,
        latency_ms=latency_ms,
        text_preview=text_preview,
    )
    if error and not segments:
        raise RuntimeError(f"stt request failed for {wav_path.name}: {error}")
    return segments
