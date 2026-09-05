"""SRT building and reading. Segment timestamps come from whisper's `verbose_json`
response (`segments[].start/end/text`, seconds) — the same field the Rust pipeline reads
`no_speech_prob` off of, just the timing half of it.
"""

from __future__ import annotations

import re
from dataclasses import dataclass


@dataclass
class Segment:
    start: float
    end: float
    text: str


def _timestamp(seconds: float) -> str:
    if seconds < 0:
        seconds = 0.0
    ms = round(seconds * 1000)
    h, ms = divmod(ms, 3_600_000)
    m, ms = divmod(ms, 60_000)
    s, ms = divmod(ms, 1_000)
    return f"{h:02d}:{m:02d}:{s:02d},{ms:03d}"


def build(segments: list[Segment]) -> str:
    lines = []
    for i, seg in enumerate(segments, start=1):
        text = seg.text.strip()
        if not text:
            continue
        lines.append(str(i))
        lines.append(f"{_timestamp(seg.start)} --> {_timestamp(seg.end)}")
        lines.append(text)
        lines.append("")
    return "\n".join(lines) + ("\n" if lines else "")


_BLOCK_RE = re.compile(
    r"^\d+\s*\n(\d\d:\d\d:\d\d,\d\d\d) --> (\d\d:\d\d:\d\d,\d\d\d)\s*\n(.*?)(?:\n\n|\Z)",
    re.MULTILINE | re.DOTALL,
)


def parse(srt_text: str) -> list[Segment]:
    out = []
    for m in _BLOCK_RE.finditer(srt_text.replace("\r\n", "\n") + "\n\n"):
        start_str, end_str, text = m.groups()
        out.append(Segment(start=_seconds(start_str), end=_seconds(end_str), text=text.strip()))
    return out


def _seconds(ts: str) -> float:
    h, m, rest = ts.split(":")
    s, ms = rest.split(",")
    return int(h) * 3600 + int(m) * 60 + int(s) + int(ms) / 1000


def to_plain_lines(srt_text: str) -> str:
    """`[hh:mm:ss] sentence` per line — the browsable/copyable form of a transcript."""
    out = []
    for seg in parse(srt_text):
        ts = _timestamp(seg.start).split(",")[0]
        out.append(f"[{ts}] {seg.text}")
    return "\n".join(out)
