import os
import subprocess
from pathlib import Path

import pytest

from pywebui import audio
from pywebui.audio import FfmpegFailed, FfmpegMissing, resolve_ffmpeg

_EXE = ".exe" if os.name == "nt" else ""


def _touch_exe(path: Path) -> None:
    path.write_bytes(b"")
    if os.name != "nt":
        path.chmod(0o755)


def test_configured_directory_with_binaries_directly_inside(tmp_path: Path):
    _touch_exe(tmp_path / f"ffmpeg{_EXE}")
    _touch_exe(tmp_path / f"ffprobe{_EXE}")
    ffmpeg, ffprobe = resolve_ffmpeg(str(tmp_path))
    assert ffmpeg == tmp_path / f"ffmpeg{_EXE}"
    assert ffprobe == tmp_path / f"ffprobe{_EXE}"


def test_configured_directory_with_bin_subfolder(tmp_path: Path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _touch_exe(bin_dir / f"ffmpeg{_EXE}")
    _touch_exe(bin_dir / f"ffprobe{_EXE}")
    ffmpeg, ffprobe = resolve_ffmpeg(str(tmp_path))
    assert ffmpeg == bin_dir / f"ffmpeg{_EXE}"
    assert ffprobe == bin_dir / f"ffprobe{_EXE}"


def test_configured_path_to_the_binary_itself(tmp_path: Path):
    _touch_exe(tmp_path / f"ffmpeg{_EXE}")
    _touch_exe(tmp_path / f"ffprobe{_EXE}")
    ffmpeg, ffprobe = resolve_ffmpeg(str(tmp_path / f"ffmpeg{_EXE}"))
    assert ffmpeg == tmp_path / f"ffmpeg{_EXE}"
    assert ffprobe == tmp_path / f"ffprobe{_EXE}"


def test_missing_configured_path_raises_and_names_it(tmp_path: Path, monkeypatch):
    monkeypatch.setattr("shutil.which", lambda name: None)
    missing = tmp_path / "nowhere"
    with pytest.raises(FfmpegMissing) as exc:
        resolve_ffmpeg(str(missing))
    assert str(missing) in str(exc.value)


def test_rejected_file_raises_ffmpeg_failed_with_stderr_tail(monkeypatch, tmp_path: Path):
    def fake_run(args, **kwargs):
        raise subprocess.CalledProcessError(
            1, args, output="", stderr="moov atom not found\n"
        )

    monkeypatch.setattr(audio.subprocess, "run", fake_run)
    corrupt = tmp_path / "corrupt.m4a"
    corrupt.write_bytes(b"not really audio")

    with pytest.raises(FfmpegFailed) as exc:
        audio._run(["ffprobe", str(corrupt)], input_file=str(corrupt))

    assert "moov atom not found" in str(exc.value)
    assert str(corrupt) in str(exc.value)


def test_moov_missing_retries_as_raw_aac_and_succeeds(monkeypatch, tmp_path: Path):
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        if len(calls) == 1:
            raise subprocess.CalledProcessError(1, args, output="", stderr="moov atom not found\n")
        return subprocess.CompletedProcess(args, 0, stdout="ok", stderr="")

    monkeypatch.setattr(audio.subprocess, "run", fake_run)
    input_path = str(tmp_path / "partial.m4a")

    out = audio._run_with_moov_recovery(
        ["ffmpeg", "-y", "-i", input_path, "-f", "segment", "out-%04d.wav"],
        input_file=input_path,
    )

    assert out.stdout == "ok"
    assert len(calls) == 2
    i = calls[1].index("-i")
    assert calls[1][i - 2 : i] == ["-f", "aac"]


def test_a_different_ffmpeg_error_is_not_retried(monkeypatch, tmp_path: Path):
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        raise subprocess.CalledProcessError(1, args, output="", stderr="Invalid data found\n")

    monkeypatch.setattr(audio.subprocess, "run", fake_run)
    input_path = str(tmp_path / "garbage.m4a")

    with pytest.raises(FfmpegFailed):
        audio._run_with_moov_recovery(["ffmpeg", "-i", input_path], input_file=input_path)

    assert len(calls) == 1


def test_detect_speech_ranges_keeps_source_offsets(monkeypatch, tmp_path: Path):
    def fake_run(args, **kwargs):
        return subprocess.CompletedProcess(
            args,
            0,
            stdout="",
            stderr=(
                "[silencedetect] silence_start: 0\n"
                "[silencedetect] silence_end: 12.5 | silence_duration: 12.5\n"
                "[silencedetect] silence_start: 30\n"
            ),
        )

    monkeypatch.setattr(audio, "resolve_ffmpeg", lambda _: (Path("ffmpeg"), Path("ffprobe")))
    monkeypatch.setattr(audio.subprocess, "run", fake_run)
    source = tmp_path / "recording.m4a"
    source.write_bytes(b"audio")

    assert audio.detect_speech_ranges(source, 45.0) == [(12.5, 30.0)]


def test_extract_speech_parts_preserves_each_original_start(monkeypatch, tmp_path: Path):
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        Path(args[-1]).write_bytes(b"wav")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(audio, "resolve_ffmpeg", lambda _: (Path("ffmpeg"), Path("ffprobe")))
    monkeypatch.setattr(audio.subprocess, "run", fake_run)
    source = tmp_path / "recording.m4a"
    source.write_bytes(b"audio")

    chunks = audio.extract_speech_parts(source, tmp_path / "parts", [(12.5, 14.0), (30.0, 31.0)])

    assert [chunk.start_offset for chunk in chunks] == [12.5]
    assert chunks[0].duration_s == 2.5
    assert chunks[0].source_ranges == [(12.5, 14.0), (30.0, 31.0)]
    assert len(calls) == 1
    assert calls[0].count("-i") == 2
    assert calls[0][calls[0].index("-ss") + 1] == "12.500"
    assert "atrim" not in calls[0]
