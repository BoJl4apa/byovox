#!/usr/bin/env python3
"""Entrypoint the Rust daemon spawns (src/webui.rs). Reads the same config.toml byovox
uses for [stt]/[polish], serves the FastAPI app on --host:--port, and runs the audio
housekeeping loop in the background."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

if sys.version_info < (3, 11):
    print("pywebui needs Python 3.11 or newer", file=sys.stderr)
    sys.exit(2)

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from pywebui import config as webui_config  # noqa: E402
from pywebui.app import create_app  # noqa: E402
from pywebui.audio import FfmpegMissing, resolve_ffmpeg  # noqa: E402
from pywebui.housekeeping import start_background  # noqa: E402
from pywebui.storage import Storage  # noqa: E402


def main() -> None:
    parser = argparse.ArgumentParser(description="byovox upload-and-transcribe web app")
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--data-dir", required=True, type=Path)
    parser.add_argument("--host", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=8787)
    args = parser.parse_args()

    cfg = webui_config.load(args.config)
    if not (cfg.webui.stt_base_url or cfg.stt.base_url):
        print("stt.base_url and webui.stt_base_url are empty; webui needs one to transcribe", file=sys.stderr)
        sys.exit(2)
    try:
        resolve_ffmpeg(cfg.webui.ffmpeg_path)
    except FfmpegMissing as e:
        # Not fatal: the upload page still serves, so this is discoverable without a restart
        # once ffmpeg is installed or webui.ffmpeg_path is set — every upload fails loudly until then.
        print(f"warning: {e}", file=sys.stderr)

    storage = Storage(args.data_dir)
    reconciled = storage.reconcile_interrupted()
    if reconciled:
        print(f"marked {reconciled} recording(s) interrupted by the previous shutdown as errored")
    start_background(storage, cfg.webui.audio_retention_days)

    app = create_app(cfg, args.data_dir)

    import uvicorn

    uvicorn.run(app, host=args.host, port=args.port, log_level="info")


if __name__ == "__main__":
    main()
