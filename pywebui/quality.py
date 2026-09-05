"""Deterministic information quality signals for generated topic nodes."""

from __future__ import annotations

import re
from collections import Counter

_WORD_RE = re.compile(r"[\w']+", re.UNICODE)
_META_PHRASES = (
    "the provided transcript",
    "the transcript consists",
    "this transcript",
    "the speaker discusses",
)


def _words(text: str) -> list[str]:
    return [word.casefold() for word in _WORD_RE.findall(text)]


def _repetition_ratio(text: str) -> float:
    words = _words(text)
    if len(words) < 8:
        return 0.0
    counts = Counter(words)
    repeated = sum(count - 1 for count in counts.values() if count > 1)
    return min(1.0, repeated / len(words))


def score_node(title: str, body: str, summary: str, start: float | None, end: float | None) -> dict:
    """Return explainable 0-100 completeness and quality scores."""
    title_ok = bool(title.strip())
    body_words = len(_words(body))
    summary_words = len(_words(summary))
    timestamp_ok = start is not None and end is not None and end >= start
    completeness_factors = {
        "title": 15 if title_ok else 0,
        "captured_text": 35 if body_words >= 8 else round(35 * body_words / 8),
        "summary": 30 if summary_words >= 8 else round(30 * summary_words / 8),
        "timestamps": 10 if timestamp_ok else 0,
        "substance": 10 if body_words >= 40 else round(10 * body_words / 40),
    }
    repetition = _repetition_ratio(body)
    meta_phrase = any(phrase in summary.casefold() for phrase in _META_PHRASES)
    quality_factors = {
        "readable_text": 35 if body_words >= 8 else round(35 * body_words / 8),
        "useful_summary": 35 if summary_words >= 8 and not meta_phrase else 15 if summary_words else 0,
        "low_repetition": round(20 * (1.0 - repetition)) if body_words else 0,
        "timestamped": 10 if timestamp_ok else 0,
    }
    return {
        "completeness": sum(completeness_factors.values()),
        "quality": sum(quality_factors.values()),
        "completeness_factors": completeness_factors,
        "quality_factors": quality_factors,
    }
