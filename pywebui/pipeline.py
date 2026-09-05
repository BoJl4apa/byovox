"""Orchestrates one recording end to end: convert/chunk -> transcribe -> stitch raw SRT ->
polish (replaces the raw transcript for everything after) -> split into topics -> per-chunk
and whole-file summaries. Each stage updates metadata.json's status so the UI can show
progress; a stage's failure marks status "error" and stops, leaving whatever the earlier
stages already wrote in place.
"""

from __future__ import annotations

import dataclasses
import json
import time
import traceback
from pathlib import Path

from . import audio, graph, llm, noise, quality, srt, stt
from .config import Config
from .logging_json import log as log_jsonl
from .storage import Chunk, Recording, Storage, slugify


def process(cfg: Config, storage: Storage, rec: Recording) -> None:
    try:
        _process(cfg, storage, rec)
    except Exception as e:  # noqa: BLE001 - a job must record its own failure, not crash the worker
        detail = f"{type(e).__name__}: {e}\n{traceback.format_exc()}"
        rec.set_status("error", error=detail)


def _process(cfg: Config, storage: Storage, rec: Recording) -> None:
    log_path = storage.interactions_log
    recording_id = rec.root.name
    original = rec.existing_original()
    if original is None and not rec.raw_srt.exists():
        raise RuntimeError("no original audio file found")

    # A chunk here is minutes of audio and a window here is thousands of words — both need
    # far more time than a live dictation's stt.timeout_s/polish.timeout_s allow for.
    processing_stt_url = cfg.webui.stt_base_url or cfg.stt.base_url
    stt_cfg = dataclasses.replace(
        cfg.stt,
        base_url=processing_stt_url,
        timeout_s=cfg.webui.stt_timeout_s,
        language_candidates=cfg.language.candidates,
    )
    polish_cfg = dataclasses.replace(
        cfg.polish,
        timeout_s=cfg.webui.llm_timeout_s,
        prompts=cfg.webui.prompts,
        analysis_max_chars=cfg.webui.analysis_max_chars,
        analysis_overlap_ratio=cfg.webui.analysis_overlap_ratio,
    )

    rec.set_status("preparing")
    rec.update_progress(stage_detail="scanning for silence", eta_s=None)
    log_jsonl(log_path, recording_id=recording_id, stage="pipeline", event="preparing")
    work_dir = rec.audio_chunks_dir
    work_dir.mkdir(parents=True, exist_ok=True)
    if rec.speech_ranges_json.exists():
        ranges = [
            (float(item["start"]), float(item["end"]))
            for item in json.loads(rec.speech_ranges_json.read_text(encoding="utf-8"))
        ]
        duration = max((end for _, end in ranges), default=0.0)
    elif original is not None:
        try:
            duration = audio.probe_duration(original, cfg.webui.ffmpeg_path)
            ranges = audio.detect_speech_ranges(
                original,
                duration,
                threshold_db=cfg.webui.silence_threshold_db,
                min_s=cfg.webui.silence_min_s,
                ffmpeg_path=cfg.webui.ffmpeg_path,
            )
        except Exception as e:
            raise RuntimeError(f"stage 1 silence detection failed: {e}") from e
        rec.speech_ranges_json.write_text(
            json.dumps(
                [{"start": start, "end": end} for start, end in ranges],
                indent=2,
            ),
            encoding="utf-8",
        )
        log_jsonl(
            log_path,
            recording_id=recording_id,
            stage="pipeline",
            event="speech_ranges",
            ranges=len(ranges),
            duration_s=duration,
        )
    else:
        ranges = []
        duration = 0.0
    total_speech_s = sum(end - start for start, end in ranges)
    rec.update_progress(total_speech_s=total_speech_s, eta_s=None)
    rec.set_status("transcribing")
    log_jsonl(log_path, recording_id=recording_id, stage="pipeline", event="transcribing")
    chunk_manifest = work_dir / "manifest.json"
    manifest_is_valid = False
    if chunk_manifest.exists():
        manifest = json.loads(chunk_manifest.read_text(encoding="utf-8"))
        manifest_is_valid = all((work_dir / item["name"]).is_file() for item in manifest)
        if not manifest_is_valid:
            prior_meta = rec.read_metadata()
            manifest_is_valid = rec.raw_srt.exists() and prior_meta.completed_chunks >= len(manifest)
    if manifest_is_valid:
        chunks = [
            audio.Chunk(
                work_dir / item["name"],
                item["start"],
                item["duration"],
                [tuple(pair) for pair in item.get("source_ranges", [])],
            )
            for item in manifest
        ]
    elif original is not None:
        if chunk_manifest.exists():
            chunk_manifest.unlink()
        try:
            chunks = audio.extract_speech_parts(
                original, work_dir, ranges, ffmpeg_path=cfg.webui.ffmpeg_path, name_prefix=f"{recording_id}-"
            )
        except Exception as e:
            raise RuntimeError(f"stage 1 speech extraction failed: {e}") from e
        chunk_manifest.write_text(
            json.dumps(
                [
                    {
                        "name": c.path.name,
                        "start": c.start_offset,
                        "duration": c.duration_s,
                        "source_ranges": c.source_ranges,
                    }
                    for c in chunks
                ],
                indent=2,
            ),
            encoding="utf-8",
        )
    completed_count = min(rec.read_metadata().completed_chunks, len(chunks))
    rec.update_progress(
        stage_detail=f"transcribing {completed_count} / {len(chunks)} chunks",
        total_chunks=len(chunks), completed_chunks=completed_count,
    )
    segments: list[srt.Segment] = []
    if rec.raw_srt.exists() and completed_count:
        segments = srt.parse(rec.raw_srt.read_text(encoding="utf-8"))
    started = time.monotonic()
    for completed, chunk in enumerate(chunks[completed_count:], start=completed_count + 1):
        segments.extend(
            stt.transcribe_chunk(
                stt_cfg,
                chunk.path,
                chunk.start_offset,
                log_path,
                recording_id,
                source_ranges=chunk.source_ranges,
            )
        )
        rec.raw_srt.write_text(srt.build(segments), encoding="utf-8")
        processed_s = min(total_speech_s, sum(c.duration_s for c in chunks[:completed]))
        elapsed = time.monotonic() - started
        eta = elapsed / (completed - completed_count) * (len(chunks) - completed) if completed > completed_count else None
        rec.update_progress(
            stage_detail=f"transcribing {completed} / {len(chunks)} chunks",
            completed_chunks=completed,
            processed_speech_s=processed_s,
            eta_s=eta,
        )
    rec.cleanup_audio_chunks()

    if rec.final_srt.exists():
        polished_segments = srt.parse(rec.final_srt.read_text(encoding="utf-8"))
        rec.update_progress(stage_detail="resuming from polished transcript", eta_s=None)
    else:
        rec.set_status("polishing")
        rec.update_progress(stage_detail="polishing transcript", eta_s=None)
        clean_segments, noise_segments = noise.classify(segments)
        rec.noise_json.write_text(json.dumps(noise_segments, ensure_ascii=False, indent=2), encoding="utf-8")
        log_jsonl(
            log_path,
            recording_id=recording_id,
            stage="pipeline",
            event="noise_classified",
            noise_count=len(noise_segments),
        )
        sentences = [s.text for s in clean_segments]
        cleaned = llm.polish_transcript(
            polish_cfg, cfg.stt.glossary(), sentences, log_path, recording_id
        )
        polished_segments = [
            srt.Segment(start=s.start, end=s.end, text=c) for s, c in zip(clean_segments, cleaned)
        ]
        rec.final_srt.write_text(srt.build(polished_segments), encoding="utf-8")

    rec.set_status("analyzing")
    rec.update_progress(stage_detail="splitting topics", eta_s=None)
    timestamped_lines = srt.to_plain_lines(rec.final_srt.read_text(encoding="utf-8")).splitlines()
    topics = llm.split_into_topics(polish_cfg, timestamped_lines, log_path, recording_id)

    rec.set_status("summarizing")
    rec.update_progress(stage_detail="writing summaries", eta_s=None)
    meta = rec.read_metadata()
    meta.duration_s = polished_segments[-1].end if polished_segments else 0.0
    chunk_records = []
    for i, topic in enumerate(topics, start=1):
        start_line = max(1, topic["start_line"])
        end_line = min(len(timestamped_lines), max(start_line, topic["end_line"]))
        body = "\n".join(timestamped_lines[start_line - 1 : end_line])
        slug = slugify(topic["title"])
        rec.chunk_txt(i, slug).write_text(body, encoding="utf-8")
        chunk_summary = llm.summarize(polish_cfg, body, log_path, recording_id)
        rec.chunk_summary_txt(i, slug).write_text(chunk_summary, encoding="utf-8")
        topic_segments = polished_segments[start_line - 1 : end_line]
        topic_start = topic_segments[0].start if topic_segments else None
        topic_end = topic_segments[-1].end if topic_segments else None
        scores = quality.score_node(topic["title"], body, chunk_summary, topic_start, topic_end)
        chunk_records.append(
            Chunk(
                index=i,
                title=topic["title"],
                start=start_line,
                end=end_line,
                slug=slug,
                start_seconds=topic_start,
                end_seconds=topic_end,
                completeness=scores["completeness"],
                quality=scores["quality"],
                completeness_factors=scores["completeness_factors"],
                quality_factors=scores["quality_factors"],
            )
        )
    meta.chunks = chunk_records
    rec.write_metadata(meta)

    full_text = "\n".join(timestamped_lines)
    whole_summary = llm.summarize(polish_cfg, full_text, log_path, recording_id)
    rec.summary_txt.write_text(whole_summary, encoding="utf-8")
    meta.display_name = _display_name(whole_summary, recording_id)
    rec.write_metadata(meta)
    for chunk in chunk_records:
        chunk_summary = rec.chunk_summary_txt(chunk.index, chunk.slug).read_text(encoding="utf-8")
        graph.update(
            rec.graph_json,
            {
                "id": f"{recording_id}:{chunk.index}",
                "recording_id": recording_id,
                "title": chunk.title,
                "summary": chunk_summary,
                "start": chunk.start,
                "end": chunk.end,
                "archived": chunk.archived,
                "index": chunk.index,
                "quality": chunk.quality,
                "completeness": chunk.completeness,
            },
            relation=lambda current, previous: llm.relationship_score(
                polish_cfg, current, previous, log_path, recording_id
            ),
        )

    rec.set_status("done")
    rec.update_progress(stage_detail="complete", eta_s=0.0)


def _display_name(summary: str, recording_id: str) -> str:
    for line in summary.splitlines():
        clean = line.strip().lstrip("-#* ")
        if clean:
            return clean if len(clean) <= 72 else clean[:69].rstrip(" ,;:-") + "..."
    return recording_id


def combined_download_text(rec: Recording) -> str:
    parts = []
    if rec.summary_txt.exists():
        parts.append("=== Summary ===\n" + rec.summary_txt.read_text(encoding="utf-8"))
    meta = rec.read_metadata()
    for c in meta.chunks:
        path = rec.chunk_txt(c.index, c.slug)
        summary_path = rec.chunk_summary_txt(c.index, c.slug)
        parts.append(f"=== {c.title} ===")
        if summary_path.exists():
            parts.append(summary_path.read_text(encoding="utf-8"))
        if path.exists():
            parts.append(path.read_text(encoding="utf-8"))
    if rec.final_srt.exists():
        parts.append("=== Full transcript ===\n" + srt.to_plain_lines(rec.final_srt.read_text(encoding="utf-8")))
    return "\n\n".join(parts) + "\n"
