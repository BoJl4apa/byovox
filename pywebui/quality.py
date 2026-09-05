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


def is_hallucination_or_noise(text: str, summary: str) -> bool:
    """Detect if content is likely a hallucination or noise that should be filtered.
    
    Returns True if the content exhibits signs of:
    - Extremely high repetition (> 65%)
    - Generic meta-phrase summary
    - Very short text with minimal unique words
    """
    if _repetition_ratio(text) > 0.65:
        return True
    
    # Meta-phrase summaries indicate failed processing
    if any(phrase in summary.casefold() for phrase in _META_PHRASES):
        return True
    
    # Too many repeated words relative to content length
    words = _words(text)
    if len(words) > 10:
        counts = Counter(words)
        most_common_word_count = max(counts.values()) if counts else 0
        if most_common_word_count > len(words) * 0.5:  # One word is > 50% of content
            return True
    
    return False


def score_node(title: str, body: str, summary: str, start: float | None, end: float | None) -> dict:
    """Return explainable 0-100 completeness and quality scores.
    
    Rewards substantive content (longer, more detailed).
    Penalizes short fragments, empty summaries, and generic meta-phrases.
    """
    title_ok = bool(title.strip())
    body_words = len(_words(body))
    summary_words = len(_words(summary))
    timestamp_ok = start is not None and end is not None and end >= start
    
    # Completeness: rewards having all components
    completeness_factors = {
        "title": 15 if title_ok else 0,
        "captured_text": 35 if body_words >= 12 else round(35 * min(body_words, 12) / 12),
        "summary": 30 if summary_words >= 8 else round(30 * min(summary_words, 8) / 8),
        "timestamps": 10 if timestamp_ok else 0,
        "substance": 10 if body_words >= 40 else round(10 * min(body_words, 40) / 40),
    }
    
    # Quality: rewards substantive, well-summarized content; penalizes short/generic
    repetition = _repetition_ratio(body)
    meta_phrase = any(phrase in summary.casefold() for phrase in _META_PHRASES)
    has_empty_summary = summary_words < 4
    
    # Text quality: good content gets full credit, very short gets less
    if body_words < 4:
        text_quality = 0  # Too short to be meaningful
    elif body_words < 8:
        text_quality = round(20 * body_words / 8)  # 4->10, 8->20
    elif body_words >= 20:
        text_quality = 35  # Full credit for substantial text
    else:
        text_quality = round(21 + (15 * (body_words - 8) / 12))  # 8->21, 20->36 (cap at 35)
    
    # Summary quality: penalize empty/generic, reward meaningful
    if has_empty_summary or meta_phrase:
        summary_quality = 0  # No credit for empty/generic
    elif summary_words >= 8:
        summary_quality = 36  # Slightly higher credit
    else:
        summary_quality = round(36 * summary_words / 8)  # Scale up to 36
    
    quality_factors = {
        "readable_text": text_quality,
        "useful_summary": summary_quality,
        "low_repetition": round(20 * (1.0 - repetition)) if body_words else 0,
        "timestamped": 10 if timestamp_ok else 0,
    }
    return {
        "completeness": sum(completeness_factors.values()),
        "quality": sum(quality_factors.values()),
        "completeness_factors": completeness_factors,
        "quality_factors": quality_factors,
    }
