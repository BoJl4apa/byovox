"""ffmpeg wrappers: probe a recording's duration, and split+convert it into ~10-minute
16 kHz mono WAV chunks for the STT endpoint. ffmpeg/ffprobe are required (`webui.ffmpeg_path`
or PATH) — this is the one hard external-binary dependency this feature adds (byovox itself
has none)."""

from __future__ import annotations

import os
import re
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

CHUNK_SECONDS = 600  # ~10 minutes; keeps each STT request well under most server limits
_EXE = ".exe" if os.name == "nt" else ""
_SILENCE_START = re.compile(r"silence_start:\s*([0-9.]+)")
_SILENCE_END = re.compile(r"silence_end:\s*([0-9.]+)")


class FfmpegMissing(RuntimeError):
    pass


class FfmpegFailed(RuntimeError):
    """ffmpeg/ffprobe ran but refused the file even after the moov-atom recovery attempt
    below — genuinely unreadable, not just an interrupted recording."""


# A phone recorder killed mid-recording (app crash, storage full, call ended the app) leaves
# an M4A/MP4 whose audio payload (`mdat`) is intact but whose index (`moov`) was never
# written, since recorders write it last. ffmpeg's mp4 demuxer needs `moov` to know where
# frames start; forcing the raw AAC (ADTS/LOAS) demuxer instead reads the same payload
# without it, so the recording transcribes with nothing rejected or repaired by hand.
_MOOV_MISSING = "moov atom not found"


def _run(args: list[str], input_file: str) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(args, capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as e:
        # The exit code alone (`e` stringified) says nothing; the reason ffmpeg gives is in
        # its last stderr lines.
        tail = "\n".join((e.stderr or "").strip().splitlines()[-5:])
        raise FfmpegFailed(f"{Path(args[0]).name} rejected {input_file}: {tail or 'no output'}") from e


def _run_with_moov_recovery(args: list[str], input_file: str) -> subprocess.CompletedProcess:
    """`args` with `-i input_file` somewhere in it: tries as given, then — only for a
    missing-moov failure — again with the input forced to the raw AAC demuxer."""
    try:
        return _run(args, input_file)
    except FfmpegFailed as e:
        if _MOOV_MISSING not in str(e):
            raise
        i = args.index("-i")
        recovered = args[:i] + ["-f", "aac"] + args[i:]
        return _run(recovered, input_file)


def resolve_ffmpeg(configured_path: str = "") -> tuple[Path, Path]:
    """(ffmpeg, ffprobe) paths: `configured_path` first — a directory holding both, a
    directory whose `bin` subfolder does (the common Windows release-zip layout), or the
    `ffmpeg` executable itself with `ffprobe` expected beside it — else PATH."""
    dirs = []
    if configured_path:
        p = Path(configured_path)
        dirs.append(p.parent if p.is_file() else p)
        dirs.append(p / "bin")
    for d in dirs:
        ffmpeg = d / f"ffmpeg{_EXE}"
        ffprobe = d / f"ffprobe{_EXE}"
        if ffmpeg.is_file() and ffprobe.is_file():
            return ffmpeg, ffprobe
    on_path = shutil.which("ffmpeg"), shutil.which("ffprobe")
    if on_path[0] and on_path[1]:
        return Path(on_path[0]), Path(on_path[1])
    where = f" (checked {configured_path} and PATH)" if configured_path else " on PATH"
    raise FfmpegMissing(
        f"ffmpeg/ffprobe not found{where}: install ffmpeg, or set webui.ffmpeg_path"
    )


def probe_duration(path: Path, ffmpeg_path: str = "") -> float:
    _, ffprobe = resolve_ffmpeg(ffmpeg_path)
    out = _run_with_moov_recovery(
        [
            str(ffprobe),
            "-v",
            "error",
            "-i",
            str(path),
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ],
        input_file=str(path),
    )
    # The raw-AAC recovery path has no container duration to report ("N/A"): harmless here,
    # since this return value is only ever used to decide whether the file is readable at
    # all, never for chunk math.
    return float(out.stdout.strip()) if out.stdout.strip() not in ("", "N/A") else 0.0


@dataclass
class Chunk:
    path: Path
    start_offset: float
    duration_s: float
    source_ranges: list[tuple[float, float]] = field(default_factory=list)


def merge_speech_ranges(
    ranges: list[tuple[float, float]], max_speech_seconds: float = CHUNK_SECONDS
) -> list[list[tuple[float, float]]]:
    """Group speech intervals into larger Whisper requests by compressed speech duration."""
    batches: list[list[tuple[float, float]]] = []
    current: list[tuple[float, float]] = []
    current_duration = 0.0
    for start, end in ranges:
        remaining = end - start
        while remaining > 0:
            capacity = max_speech_seconds - current_duration
            if current and capacity <= 0:
                batches.append(current)
                current, current_duration = [], 0.0
                capacity = max_speech_seconds
            take = min(remaining, capacity)
            part_end = start + take
            current.append((start, part_end))
            current_duration += take
            start = part_end
            remaining -= take
            if current_duration >= max_speech_seconds - 0.001:
                batches.append(current)
                current, current_duration = [], 0.0
    if current:
        batches.append(current)
    return batches


def detect_speech_ranges(
    input_path: Path,
    duration: float,
    threshold_db: float = -35.0,
    min_s: float = 1.0,
    ffmpeg_path: str = "",
) -> list[tuple[float, float]]:
    """Find non-silent intervals while retaining their positions in the source timeline."""
    ffmpeg, _ = resolve_ffmpeg(ffmpeg_path)
    null_output = "NUL" if os.name == "nt" else os.devnull
    result = _run_with_moov_recovery(
        [
            str(ffmpeg),
            "-hide_banner",
            "-i",
            str(input_path),
            "-af",
            f"silencedetect=noise={threshold_db}dB:d={min_s}",
            "-f",
            "null",
            null_output,
        ],
        input_file=str(input_path),
    )
    events = []
    for line in result.stderr.splitlines():
        start = _SILENCE_START.search(line)
        end = _SILENCE_END.search(line)
        if start:
            events.append(("start", float(start.group(1))))
        if end:
            events.append(("end", float(end.group(1))))

    ranges: list[tuple[float, float]] = []
    cursor = 0.0
    in_silence = False
    for kind, timestamp in sorted(events, key=lambda event: event[1]):
        timestamp = max(cursor, min(duration, timestamp))
        if kind == "start" and not in_silence:
            if timestamp > cursor:
                ranges.append((cursor, timestamp))
            in_silence = True
        elif kind == "end" and in_silence:
            cursor = timestamp
            in_silence = False
    if not in_silence and cursor < duration:
        ranges.append((cursor, duration))
    return [(start, end) for start, end in ranges if end - start >= 0.05]


def extract_speech_parts(
    input_path: Path,
    out_dir: Path,
    ranges: list[tuple[float, float]],
    ffmpeg_path: str = "",
    chunk_seconds: int = CHUNK_SECONDS,
    name_prefix: str = "",
) -> list[Chunk]:
    """Extract merged speech batches into WAVs while preserving source-time mappings."""
    ffmpeg, _ = resolve_ffmpeg(ffmpeg_path)
    out_dir.mkdir(parents=True, exist_ok=True)
    chunks: list[Chunk] = []
    index = 0
    for batch in merge_speech_ranges(ranges, chunk_seconds):
        length = sum(end - start for start, end in batch)
        path = out_dir / f"{name_prefix}chunk-{index:04d}.wav"
        filters = []
        labels = []
        args = [str(ffmpeg), "-y"]
        for part_index, (part_start, part_end) in enumerate(batch):
            # Input-side seeking avoids decoding the entire recording for every batch.
            # The short overlap around compressed-audio seek points is discarded by the
            # requested duration and does not affect the restored source timestamps.
            args.extend(
                [
                    "-ss",
                    f"{part_start:.3f}",
                    "-t",
                    f"{part_end - part_start:.3f}",
                    "-i",
                    str(input_path),
                ]
            )
            filters.append(f"[{part_index}:a]asetpts=PTS-STARTPTS[a{part_index}]")
            labels.append(f"[a{part_index}]")
        filter_graph = ";".join(filters) + ";" + "".join(labels)
        # Concatenate all parts, then apply voice enhancement filters:
        # - highpass: remove low-frequency rumble, hum, wind noise (80 Hz cutoff)
        # - lowpass: remove high-frequency hiss, noise (8 kHz cutoff)
        # - compand: compress dynamic range (boost weak voices, reduce loud peaks)
        # - anormalizer: normalize overall levels while preserving dynamic balance
        filter_graph += f"concat=n={len(batch)}:v=0:a=1[concat];" \
                        "[concat]highpass=f=80:poles=1[hp];" \
                        "[hp]lowpass=f=8000:poles=1[lp];" \
                        "[lp]compand=attacks=0.005:decays=0.1:points=-80/-80|-4.5/-2|-1/-1|0/0:soft-knee=6:gain=4[comp];" \
                        "[comp]anormalizer=s=0.1[out]"
        args.extend(
            [
                "-filter_complex",
                filter_graph,
                "-map",
                "[out]",
                "-ar",
                "16000",
                "-ac",
                "1",
                str(path),
            ]
        )
        _run(args, input_file=str(input_path))
        chunks.append(
            Chunk(
                path=path,
                start_offset=batch[0][0],
                duration_s=length,
                source_ranges=batch,
            )
        )
        index += 1
    return chunks


def convert_and_chunk(
    input_path: Path, out_dir: Path, chunk_seconds: int = CHUNK_SECONDS, ffmpeg_path: str = ""
) -> list[Chunk]:
    """Converts to 16 kHz mono PCM WAV with voice enhancement, split into `chunk_seconds`-long pieces.
    
    Applies audio filters to enhance speech quality:
    - High-pass filter: removes low-frequency rumble and hum
    - Low-pass filter: removes high-frequency hiss
    - Compressor: boosts weak voices and reduces dynamic range
    - Normalizer: optimizes overall levels
    """
    ffmpeg, _ = resolve_ffmpeg(ffmpeg_path)
    out_dir.mkdir(parents=True, exist_ok=True)
    pattern = out_dir / "chunk-%04d.wav"
    voice_enhancement_filter = (
        "highpass=f=80:poles=1,"
        "lowpass=f=8000:poles=1,"
        "compand=attacks=0.005:decays=0.1:points=-80/-80|-4.5/-2|-1/-1|0/0:soft-knee=6:gain=4,"
        "anormalizer=s=0.1"
    )
    _run_with_moov_recovery(
        [
            str(ffmpeg),
            "-y",
            "-i",
            str(input_path),
            "-af",
            voice_enhancement_filter,
            "-ar",
            "16000",
            "-ac",
            "1",
            "-f",
            "segment",
            "-segment_time",
            str(chunk_seconds),
            "-reset_timestamps",
            "1",
            str(pattern),
        ],
        input_file=str(input_path),
    )
    chunks = []
    for i, path in enumerate(sorted(out_dir.glob("chunk-*.wav"))):
        chunks.append(Chunk(path=path, start_offset=i * chunk_seconds, duration_s=chunk_seconds))
    return chunks
