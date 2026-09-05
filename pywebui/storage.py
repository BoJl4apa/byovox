"""Recording folders: creation, listing, and the one metadata.json per recording.

No database (flat files, by design): `<data_dir>/webui/recordings/<id>/metadata.json` is
the source of truth for status and structure; the web app lists recordings by scanning the
directory. `<data_dir>/webui/logs/interactions.jsonl` is the one central log, shared by
every recording (see logging_json.py).
"""

from __future__ import annotations

import json
import os
import re
import unicodedata
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path

STATUSES = (
    "uploaded",
    "preparing",
    "transcribing",
    "polishing",
    "analyzing",
    "summarizing",
    "done",
    "error",
)

# A job runs only in this process's in-memory queue (jobs.py); one of these left over from
# before this process started can only mean it was interrupted, never that it is still
# running elsewhere.
IN_PROGRESS_STATUSES = ("uploaded", "preparing", "transcribing", "polishing", "analyzing", "summarizing")


def slugify(name: str) -> str:
    name = unicodedata.normalize("NFKD", name)
    name = re.sub(r"[^A-Za-z0-9._-]+", "-", name).strip("-")
    return (name or "recording")[:60]


@dataclass
class Chunk:
    index: int
    title: str
    start: float
    end: float
    slug: str
    start_seconds: float | None = None
    end_seconds: float | None = None
    completeness: int = 0
    quality: int = 0
    completeness_factors: dict[str, int] = field(default_factory=dict)
    quality_factors: dict[str, int] = field(default_factory=dict)
    archived: bool = False
    parent_index: int | None = None
    version: int = 1


@dataclass
class Metadata:
    id: str
    created_at: str
    original_filename: str
    display_name: str | None = None
    status: str = "uploaded"
    error: str | None = None
    duration_s: float | None = None
    stage_detail: str | None = None
    total_chunks: int = 0
    completed_chunks: int = 0
    total_speech_s: float = 0.0
    processed_speech_s: float = 0.0
    eta_s: float | None = None
    chunks: list[Chunk] = field(default_factory=list)

    def to_json(self) -> str:
        return json.dumps(asdict(self), ensure_ascii=False, indent=2)


class Recording:
    """One upload's folder on disk. Every path helper lives here so pipeline.py and app.py
    never hand-build a filename and drift apart."""

    def __init__(self, root: Path):
        self.root = root

    @property
    def original_glob(self) -> str:
        return "original.*"

    def original_path(self, suffix: str) -> Path:
        return self.root / f"original{suffix}"

    def existing_original(self) -> Path | None:
        matches = sorted(self.root.glob(self.original_glob))
        return matches[0] if matches else None

    @property
    def raw_srt(self) -> Path:
        return self.root / "transcript.raw.srt"

    @property
    def final_srt(self) -> Path:
        return self.root / "transcript.srt"

    @property
    def summary_txt(self) -> Path:
        return self.root / "summary.txt"

    @property
    def speech_ranges_json(self) -> Path:
        return self.root / "speech-ranges.json"
    
    @property
    def noise_json(self) -> Path:
        return self.root / "noise.json"

    @property
    def graph_json(self) -> Path:
        return self.root.parent.parent / "graph.json"

    @property
    def refined_json(self) -> Path:
        return self.root / "refined.json"

    @property
    def refinement_history_json(self) -> Path:
        return self.root / "refinement-history.json"

    def read_refined(self) -> dict[str, str]:
        if not self.refined_json.exists():
            return {}
        return json.loads(self.refined_json.read_text(encoding="utf-8"))

    def write_refined(self, target: str, text: str) -> None:
        refined = self.read_refined()
        refined[target] = text
        self.refined_json.write_text(json.dumps(refined, ensure_ascii=False, indent=2), encoding="utf-8")

    def read_refinement_history(self) -> dict[str, list[dict[str, str]]]:
        if not self.refinement_history_json.exists():
            return {}
        return json.loads(self.refinement_history_json.read_text(encoding="utf-8"))

    def add_refinement_history(self, target: str, instruction: str, original: str, revised: str) -> None:
        history = self.read_refinement_history()
        history.setdefault(target, []).append(
            {"instruction": instruction, "original": original, "revised": revised}
        )
        self.refinement_history_json.write_text(
            json.dumps(history, ensure_ascii=False, indent=2), encoding="utf-8"
        )

    @property
    def chunks_dir(self) -> Path:
        return self.root / "chunks"

    @property
    def audio_chunks_dir(self) -> Path:
        return self.root / "audio-chunks"

    @property
    def metadata_path(self) -> Path:
        return self.root / "metadata.json"

    def chunk_txt(self, index: int, slug: str) -> Path:
        return self.chunks_dir / f"{index:02d}-{slug}.txt"

    def chunk_summary_txt(self, index: int, slug: str) -> Path:
        return self.chunks_dir / f"{index:02d}-{slug}.summary.txt"

    def read_metadata(self) -> Metadata:
        raw = json.loads(self.metadata_path.read_text(encoding="utf-8"))
        raw["chunks"] = [Chunk(**c) for c in raw.get("chunks", [])]
        return Metadata(**raw)

    def write_metadata(self, meta: Metadata) -> None:
        temp = self.metadata_path.with_suffix(".json.tmp")
        temp.write_text(meta.to_json(), encoding="utf-8")
        os.replace(temp, self.metadata_path)

    def set_status(self, status: str, error: str | None = None) -> None:
        meta = self.read_metadata()
        meta.status = status
        meta.error = error
        self.write_metadata(meta)

    def update_progress(self, **changes: object) -> None:
        meta = self.read_metadata()
        for name, value in changes.items():
            if not hasattr(meta, name):
                raise ValueError(f"unknown progress field: {name}")
            setattr(meta, name, value)
        self.write_metadata(meta)

    def cleanup_audio_chunks(self) -> None:
        """Remove generated WAV payloads while keeping the manifest for checkpoints."""
        if not self.audio_chunks_dir.exists():
            return
        for path in self.audio_chunks_dir.glob("*.wav"):
            path.unlink(missing_ok=True)

    def reset_for_recalculation(self) -> None:
        """Clear generated topics and summaries while retaining the raw Whisper transcript."""
        for f in self.chunks_dir.glob("*"):
            f.unlink()
        self.final_srt.unlink(missing_ok=True)
        self.summary_txt.unlink(missing_ok=True)
        self.refined_json.unlink(missing_ok=True)
        self.refinement_history_json.unlink(missing_ok=True)
        meta = self.read_metadata()
        meta.status = "uploaded"
        meta.error = None
        meta.stage_detail = "recalculating from the raw transcript"
        meta.eta_s = None
        meta.chunks = []
        self.write_metadata(meta)

    def reset_for_retry(self) -> None:
        """Clear downstream results while retaining audio and transcription checkpoints."""
        for f in self.chunks_dir.glob("*"):
            f.unlink()
        self.final_srt.unlink(missing_ok=True)
        self.summary_txt.unlink(missing_ok=True)
        self.refined_json.unlink(missing_ok=True)
        self.refinement_history_json.unlink(missing_ok=True)
        meta = self.read_metadata()
        meta.status = "uploaded"
        meta.error = None
        meta.duration_s = None
        meta.stage_detail = "resuming from the last checkpoint"
        meta.eta_s = None
        meta.chunks = []
        self.write_metadata(meta)


class Storage:
    def __init__(self, data_dir: Path):
        self.recordings_dir = data_dir / "webui" / "recordings"
        self.logs_dir = data_dir / "webui" / "logs"
        self.recordings_dir.mkdir(parents=True, exist_ok=True)
        self.logs_dir.mkdir(parents=True, exist_ok=True)

    @property
    def interactions_log(self) -> Path:
        return self.logs_dir / "interactions.jsonl"

    def create(self, original_filename: str) -> Recording:
        now = datetime.now(timezone.utc)
        stamp = now.strftime("%Y%m%d-%H%M%S")
        slug = slugify(Path(original_filename).stem)
        folder_id = f"{stamp}-{slug}"
        root = self.recordings_dir / folder_id
        root.mkdir(parents=True, exist_ok=False)
        (root / "chunks").mkdir(exist_ok=True)
        rec = Recording(root)
        rec.write_metadata(
            Metadata(
                id=folder_id,
                created_at=now.isoformat(),
                original_filename=original_filename,
            )
        )
        return rec

    def get(self, recording_id: str) -> Recording | None:
        # Reject anything that is not a plain folder name under recordings_dir — the id
        # comes from a URL path segment, and `..`/absolute paths must never escape it.
        if "/" in recording_id or "\\" in recording_id or recording_id in ("", ".", ".."):
            return None
        root = self.recordings_dir / recording_id
        if not root.is_dir() or not (root / "metadata.json").exists():
            return None
        return Recording(root)

    def list(self) -> list[Recording]:
        out = []
        for child in self.recordings_dir.iterdir():
            if child.is_dir() and (child / "metadata.json").exists():
                out.append(Recording(child))
        out.sort(key=lambda r: r.root.name, reverse=True)
        return out

    def reconcile_interrupted(self) -> int:
        """Marks every recording left in an in-progress status as errored: reached once at
        startup, since a daemon restart kills the job that was working on it without a
        chance to record its own failure otherwise, leaving it stuck with no Retry button."""
        marked = 0
        for rec in self.list():
            meta = rec.read_metadata()
            if meta.status in IN_PROGRESS_STATUSES:
                rec.set_status("error", error="interrupted by a restart before finishing — retry to resume")
                marked += 1
        return marked
