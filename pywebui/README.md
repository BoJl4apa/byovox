# pywebui

The upload-and-transcribe web app: a mobile-first page for uploading a long recording
(M4A/MP3) and getting back a timestamped transcript, topic/story chunks and summaries. A
separate feature from byovox's push-to-talk dictation — see the main
[README.md](../README.md#upload--transcribe-a-long-recording-web-ui) for how the two relate.

Enabled and spawned by the Rust daemon (`src/webui.rs`) when `[webui] enabled = true` in
`config.toml`; this package never runs standalone in normal use, but `server.py` can be run
directly for development:

```sh
python -m venv .venv
.venv/Scripts/activate   # .venv/bin/activate on Linux/macOS
pip install -r requirements.txt
python server.py --config ~/.config/byovox/config.toml --data-dir ./data --host 127.0.0.1 --port 8787
```

## Requirements

- Python 3.11+ (uses `tomllib`)
- [ffmpeg](https://ffmpeg.org/download.html) and `ffprobe` on PATH — used to convert
  M4A/MP3 to 16 kHz mono WAV and split long recordings into chunks before sending them to STT
- The same `[stt]` and `[polish]` endpoints `config.toml` already points at

## How a recording is processed

1. **Convert & chunk** (`audio.py`): ffmpeg splits the upload into ~10-minute WAV chunks.
2. **Transcribe** (`stt.py`): each chunk goes to `{stt.base_url}/audio/transcriptions`
   requesting `verbose_json`, giving per-sentence timestamps; segments are stitched into
   `transcript.raw.srt`.
3. **Polish** (`llm.py`): the raw transcript is cleaned up in windows through
   `{polish.base_url}/chat/completions`, using rules that mirror `src/polish.rs`'s built-in
   prompt. The result replaces the raw draft as `transcript.srt` — nothing after this step
   ever rewrites it.
4. **Split into topics**: the polished, timestamped transcript is scanned for distinct
   topics/stories/conversations, each written as its own file under `chunks/`.
5. **Summarize**: a summary is written per chunk and for the whole recording.

Every Whisper/LLM call is logged as one JSON line in
`<data-dir>/webui/logs/interactions.jsonl` (`logging_json.py`) — request stage, model,
status, latency, and (for STT) a short text preview — for tracing what happened without
re-running anything.

## Storage

`<data-dir>/webui/recordings/<timestamp>-<slug>/`:

```
original.m4a            # deleted after webui.audio_retention_days
transcript.raw.srt       # first whisper-only pass, kept for audit
transcript.srt           # polished, final — never rewritten again
chunks/01-<slug>.txt
chunks/01-<slug>.summary.txt
summary.txt
metadata.json            # status, chunk list, timestamps
```

No database: the web app lists recordings by scanning this directory, per-recording state
lives in `metadata.json`.

For separate Whisper processes, set `webui.stt_base_url` in the main byovox config to the
processing server URL, for example `http://127.0.0.1:8771/v1`. The push-to-talk server keeps
using `stt.base_url`. On Windows, `whisper-servers.bat` contains the editable model paths,
ports, languages, and extra whisper-server arguments used by `start-byovox.bat`.

## Tests

```sh
pip install -r requirements.txt pytest
pytest tests
```
