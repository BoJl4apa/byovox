"""Small incremental relationship graph for topic nodes."""

from __future__ import annotations

import json
import re
from pathlib import Path

_WORD_RE = re.compile(r"[\w']+", re.UNICODE)
_STOP = {"the", "and", "that", "this", "with", "from", "about", "were", "есть", "это"}
MIN_RELATIONSHIP_STRENGTH = 0.35
_NOISE_TERMS = {"ambient", "indistinct", "background", "noise", "inaudible", "unclear"}


def _terms(text: str) -> set[str]:
    return {word.casefold() for word in _WORD_RE.findall(text) if len(word) > 2 and word.casefold() not in _STOP}


def _eligible(node: dict[str, object]) -> bool:
    if node.get("archived") or node.get("noise"):
        return False
    title = str(node.get("title", "")).casefold()
    summary = str(node.get("summary", "")).casefold()
    if any(term in title.split() for term in _NOISE_TERMS):
        return False
    if node.get("quality", 100) < 35 or node.get("completeness", 100) < 30:
        return False
    return bool(_terms(f"{title} {summary}"))


def update(path: Path, node: dict[str, object], relation=None) -> dict[str, object]:
    data = {"version": 1, "nodes": [], "edges": []}
    if path.exists():
        data.update(json.loads(path.read_text(encoding="utf-8")))
    data["nodes"] = [old for old in data["nodes"] if old.get("id") != node["id"]]
    data["nodes"].append(node)
    current_terms = _terms(f"{node.get('title', '')} {node.get('summary', '')}")
    data["edges"] = [
        edge for edge in data["edges"]
        if edge["source"] != node["id"] and edge["target"] != node["id"]
        and edge.get("strength", 0) >= MIN_RELATIONSHIP_STRENGTH
    ]
    for old in data["nodes"]:
        if old["id"] == node["id"] or not _eligible(node) or not _eligible(old):
            continue
        old_terms = _terms(f"{old.get('title', '')} {old.get('summary', '')}")
        union = current_terms | old_terms
        strength = len(current_terms & old_terms) / len(union) if union else 0.0
        if strength >= 0.12:
            if relation is not None:
                try:
                    strength = max(0.0, min(1.0, float(relation(node, old))))
                except (TypeError, ValueError, RuntimeError):
                    pass
            if strength >= MIN_RELATIONSHIP_STRENGTH:
                data["edges"].append({"source": node["id"], "target": old["id"], "strength": round(strength, 3)})
    data["edges"] = [edge for edge in data["edges"] if edge.get("strength", 0) >= MIN_RELATIONSHIP_STRENGTH]
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2), encoding="utf-8")
    return data
