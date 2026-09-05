import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

from pywebui import llm, srt, stt
from pywebui.config import PolishConfig, SttConfig


class _Handler(BaseHTTPRequestHandler):
    response_body = b"{}"
    response_status = 200
    last_request = {}

    def do_POST(self):  # noqa: N802 - http.server's naming
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        _Handler.last_request = {
            "path": self.path,
            "headers": dict(self.headers.items()),
            "body": body,
        }
        self.send_response(_Handler.response_status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(_Handler.response_body)

    def log_message(self, *a):  # silence
        pass


@pytest.fixture()
def mock_server():
    server = HTTPServer(("127.0.0.1", 0), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield server
    server.shutdown()
    thread.join()


def test_stt_sends_multipart_with_model_and_verbose_json(mock_server, tmp_path: Path):
    _Handler.response_body = json.dumps(
        {"text": "hello world", "segments": [{"start": 0.0, "end": 1.0, "text": "hello world"}]}
    ).encode()
    port = mock_server.server_address[1]
    cfg = SttConfig(base_url=f"http://127.0.0.1:{port}", model="whisper-1", timeout_s=5)
    wav = tmp_path / "clip.wav"
    wav.write_bytes(b"RIFF....")
    log_path = tmp_path / "interactions.jsonl"

    segments = stt.transcribe_chunk(cfg, wav, start_offset=10.0, log_path=log_path, recording_id="r1")

    assert segments == [srt.Segment(start=10.0, end=11.0, text="hello world")]
    req = _Handler.last_request
    assert req["path"] == "/audio/transcriptions"
    assert b'name="model"' in req["body"]
    assert b"whisper-1" in req["body"]
    assert b'name="response_format"' in req["body"]
    assert b"verbose_json" in req["body"]
    assert log_path.exists()


def test_stt_sends_language_candidates(mock_server, tmp_path: Path):
    _Handler.response_body = json.dumps({"text": "hello", "segments": []}).encode()
    port = mock_server.server_address[1]
    cfg = SttConfig(
        base_url=f"http://127.0.0.1:{port}",
        language_candidates=["en", "ru", "he"],
        timeout_s=5,
    )
    wav = tmp_path / "clip.wav"
    wav.write_bytes(b"RIFF....")
    stt.transcribe_chunk(cfg, wav, 0.0, tmp_path / "events.jsonl", "r1")

    assert b'name="language_candidates"' in _Handler.last_request["body"]
    assert b"en,ru,he" in _Handler.last_request["body"]


def test_llm_chat_sends_system_and_user_messages(mock_server, tmp_path: Path):
    _Handler.response_body = json.dumps(
        {"choices": [{"message": {"content": "cleaned"}}]}
    ).encode()
    port = mock_server.server_address[1]
    cfg = PolishConfig(base_url=f"http://127.0.0.1:{port}", model="gemma3", timeout_s=5)
    log_path = tmp_path / "interactions.jsonl"

    out = llm._chat(cfg, "system prompt", "user text", log_path, "r1", "test-stage")

    assert out == "cleaned"
    req = _Handler.last_request
    body = json.loads(req["body"])
    assert body["messages"][0] == {"role": "system", "content": "system prompt"}
    assert body["messages"][1] == {"role": "user", "content": "user text"}
    assert body["model"] == "gemma3"


def test_bearer_header_sent_when_token_present(mock_server, tmp_path: Path, monkeypatch):
    _Handler.response_body = json.dumps(
        {"choices": [{"message": {"content": "ok"}}]}
    ).encode()
    port = mock_server.server_address[1]
    monkeypatch.setenv("TEST_TOKEN", "secret123")
    cfg = PolishConfig(
        base_url=f"http://127.0.0.1:{port}", model="m", api_key_env="TEST_TOKEN", timeout_s=5
    )
    log_path = tmp_path / "interactions.jsonl"

    llm._chat(cfg, "s", "u", log_path, "r1", "stage")

    assert _Handler.last_request["headers"]["Authorization"] == "Bearer secret123"
