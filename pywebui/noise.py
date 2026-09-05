"""Conservative detection of repeated transcript hallucinations and impact noise."""

from __future__ import annotations

import re
from collections import Counter

from . import srt

_WORD_RE = re.compile(r"[\w']+", re.UNICODE)


def _key(text: str) -> str:
    return " ".join(_WORD_RE.findall(text.casefold()))


def classify(segments: list[srt.Segment]) -> tuple[list[srt.Segment], list[dict[str, object]]]:
    """Separate exact/near repeated short phrases while preserving source timestamps."""
    keys = Counter(_key(segment.text) for segment in segments)
    noise: list[dict[str, object]] = []
    clean: list[srt.Segment] = []
    for segment in segments:
        key = _key(segment.text)
        words = key.split()
        repeated_short_phrase = bool(key) and len(words) <= 8 and keys[key] >= 3
        if repeated_short_phrase:
            noise.append(
                {
                    "start": segment.start,
                    "end": segment.end,
                    "text": segment.text,
                    "reason": "repeated short phrase",
                    "occurrences": keys[key],
                }
            )
        else:
            clean.append(segment)
    return clean, noise
